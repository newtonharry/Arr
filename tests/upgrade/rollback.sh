#!/bin/sh
# CI: the auto-update rollback, with the real binaries. install.sh and install.ps1 swap in a
# new release, start it, wait for /api/v1/health to report the new version and otherwise put
# the previous binary back. That only works if the previous release starts on the config and
# state the new one has run on, so this checks it, both ways round:
#
#   1. the new binary (the one under test) is installed by the CURRENT install.sh (so the
#      config is what the installer writes today, not a copy of it kept here) and writes a
#      movie and a series season through its API; then the latest published release runs on
#      that config and state and lists those titles, and the new binary runs on it again;
#   2. the latest published release is installed by the same install.sh; the new binary runs
#      on it and writes a movie and a season; the latest published release runs on it again
#      and lists them.
#
# The setup page's config (src/wizard.rs, also current) is rendered by a unit test and checked
# the same way: `arr setup`, `arr check` and `arr serve` of the latest release must accept it.
#
# "Runs" is `arr check` and `arr serve` answering /api/v1/health. The old release is downloaded
# and its .sha256 verified. A local mock stands in for Plex (install.sh reads the libraries from
# it) and for TVmaze (the season's episodes); nothing else is contacted. Needs curl, python3,
# ffprobe and cargo (for the setup page's config).
#
# Usage: sh tests/upgrade/rollback.sh path/to/new/arr      (any directory of the repository)
# Environment: ARR_OLD_BASE        where the old release lives (default: the latest GitHub release);
#                                  a file:// or http(s):// folder holding arr-<target>.tar.gz and its .sha256
#              ARR_ROLLBACK_PORT   the port to serve on (default: a free one)
#              ARR_CARGO_FLAGS     extra flags for the cargo test that renders the setup page's config (CI: --locked)
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
freeport() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=${ARR_ROLLBACK_PORT:-$(freeport)}
mockport=$(freeport)
torrentport=$(freeport)
repo=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$repo/install.sh" ] || fail "cannot find install.sh in $repo"

work=$(mktemp -d)
pid=""
mockpid=""
stop() { [ -z "$pid" ] || { kill "$pid" 2>/dev/null || true; wait "$pid" 2>/dev/null || true; pid=""; }; }
trap 'stop; [ -z "$mockpid" ] || kill "$mockpid" 2>/dev/null || true; rm -rf "$work"' EXIT

# --- the previous release, checksum verified
mkdir "$work/release"
# GitHub's release downloads sometimes answer 504 for a moment: retry rather than fail the run.
curl -fsSL --retry 5 --retry-all-errors --retry-delay 10 "$base/$asset" -o "$work/release/$asset" || fail "cannot download $base/$asset"
curl -fsSL --retry 5 --retry-all-errors --retry-delay 10 "$base/$asset.sha256" -o "$work/release/$asset.sha256" || fail "cannot download $base/$asset.sha256"
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

# The new binary as a release asset, for install.sh.
mkdir -p "$work/newrel/arr-$target"
cp "$new" "$work/newrel/arr-$target/arr"
tar -C "$work/newrel" -czf "$work/newrel/$asset" "arr-$target"
(cd "$work/newrel" && sha256sum "$asset" >"$asset.sha256")

# --- the mock: Plex's library list for install.sh, TVmaze for a season
cat >"$work/mock.py" <<'EOF'
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
SHOW = {"id": 9001, "name": "Slow Horses", "premiered": "2022-04-01", "language": "English", "runtime": 50,
        "status": "Running", "type": "Scripted", "externals": {"thetvdb": 372264, "imdb": "tt5875444"}}
EPS = [{"season": 1, "number": n, "name": "Episode %d" % n, "airstamp": "2022-04-%02dT02:00:00+00:00" % (n + 1),
        "runtime": 50, "type": "regular"} for n in range(1, 7)]
SECTIONS = """<MediaContainer size="2">
<Directory key="1" type="movie" title="Movies"><Location id="1" path="/data/movies"/></Directory>
<Directory key="2" type="show" title="TV Shows"><Location id="2" path="/data/tv"/></Directory>
</MediaContainer>"""
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def reply(self, body, kind="application/json", code=200):
        b = body.encode(); self.send_response(code); self.send_header("content-type", kind)
        self.send_header("content-length", str(len(b))); self.end_headers(); self.wfile.write(b)
    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/plex/identity": return self.reply("{}")
        if p == "/plex/library/sections": return self.reply(SECTIONS, "text/xml")
        if p.startswith("/tvmaze/lookup/shows") or p == "/tvmaze/shows/9001": return self.reply(json.dumps(SHOW))
        if p == "/tvmaze/shows/9001/episodes": return self.reply(json.dumps(EPS))
        if p == "/tvmaze/shows/9001/akas": return self.reply("[]")
        self.reply("{}", code=404)
ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
EOF
python3 -I "$work/mock.py" "$mockport" &
mockpid=$!

# --- helpers
# The real installer, as a user runs it (no service, nothing started), with the given release.
install_with() { # release-folder state-dir bin-dir
	mkdir -p "$2.data/movies" "$2.data/tv" "$2.data/downloads"
	ARR_DOWNLOAD_BASE="file://$1" sh "$repo/install.sh" --yes --no-service --no-auto-update \
		--state-dir "$2" --bin-dir "$3" --plex-url "http://127.0.0.1:$mockport/plex" --plex-token fake-plex-token \
		--movies "$2.data/movies" --tv "$2.data/tv" --downloads "$2.data/downloads" \
		--port "$port" --torrent-port "$torrentport" --upload-limit-kib 512 >"$work/install.out" 2>&1 ||
		{ cat "$work/install.out" >&2; fail "install.sh failed"; }
	# TVmaze is the one outside service a season needs: point it at the mock.
	sed -i 's#^\[tvmaze\]$#&\nbase_url = "http://127.0.0.1:'"$mockport"'/tvmaze"#' "$2/config.toml"
}

api() { # state method path [json-body]: the body on stdout; fails unless the status is 2xx
	tok=$(cat "$1/secrets/api_token")
	if [ -n "${4:-}" ]; then
		curl -fsS -X "$2" -H "Authorization: Bearer $tok" -H 'content-type: application/json' -d "$4" "http://127.0.0.1:$port$3"
	else
		curl -fsS -X "$2" -H "Authorization: Bearer $tok" "http://127.0.0.1:$port$3"
	fi
}

# `arr check`, then `arr serve` until /api/v1/health answers with this binary's version.
# mode: plain | write (add a movie and a season through the API) | read (they must be listed).
run() { # label binary state-dir mode
	label=$1 bin=$2 state=$3 mode=$4
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
	if [ "$mode" = write ]; then
		api "$state" POST /api/v1/library '{"media_id":"movie:tmdb:603","search":false,"metadata":{"title":"The Matrix","year":1999,"runtime_minutes":136,"in_cinemas":"1999-03-31"}}' >/dev/null ||
			{ cat "$work/serve.log" >&2; fail "$label: adding a movie failed"; }
		api "$state" POST /api/v1/library '{"media_id":"season:tvdb:372264:1","search":false}' >/dev/null ||
			{ cat "$work/serve.log" >&2; fail "$label: adding a season failed"; }
	fi
	if [ "$mode" = write ] || [ "$mode" = read ]; then
		api "$state" GET /api/v1/library/movies | grep -q 'The Matrix' || fail "$label: the movie isn't listed"
		api "$state" GET /api/v1/library/series | grep -q 'Slow Horses' || fail "$label: the series isn't listed"
	fi
	stop
	echo "ok: $label ($version, $mode): check passed, serve healthy"
}

# --- 1. installed by the new binary, which adds titles; the old one runs on it; then new again
s1="$work/state-new-first"
install_with "$work/newrel" "$s1" "$work/bin-new-first"
run "new installed, new runs and adds titles" "$new" "$s1" write
run "new installed, OLD runs (rollback)" "$old" "$s1" read
run "then the new one again (the update retried)" "$new" "$s1" read

# --- 2. installed by the old binary; the new one runs and adds titles; the old one runs again
s2="$work/state-old-first"
install_with "$work/release" "$s2" "$work/bin-old-first"
run "old installed, old runs" "$old" "$s2" plain
run "old installed, NEW runs and adds titles (update)" "$new" "$s2" write
run "old installed, new has run, OLD runs (rollback)" "$old" "$s2" read

# --- 3. the setup page's config, rendered by the current code, accepted by the old release
w="$work/wizard-state"
mkdir -p "$w"
chmod 700 "$w"
# shellcheck disable=SC2086
(cd "$repo" && ARR_WIZARD_CONFIG_OUT="$w/config.toml" cargo test ${ARR_CARGO_FLAGS:-} --lib the_config_the_setup_page_writes_loads >"$work/wizard-test.out" 2>&1) ||
	{ cat "$work/wizard-test.out" >&2; fail "cannot render the setup page's config"; }
[ -s "$w/config.toml" ] || fail "the setup page's config wasn't written"
chmod 600 "$w/config.toml"
mkdir -p "$w/secrets" "$w-movies" "$w-tv" "$w-downloads"
chmod 700 "$w/secrets"
ln -s "$ffprobe" "$w-ffprobe"
for f in $(grep -o '"secrets/[^"]*"' "$w/config.toml" | tr -d '"'); do
	printf '%s' "fake-secret-value-for-$(basename "$f")-0123456789" >"$w/$f"
	chmod 600 "$w/$f"
done
# Only the sockets are changed: this machine's ports, nothing exposed.
sed -i -e "s#^listen = \"127.0.0.1:8787\"#listen = \"127.0.0.1:$port\"#" -e 's#^listen = "0.0.0.0:[0-9]*"#listen = "127.0.0.1:0"#' "$w/config.toml"
"$old" setup --config "$w/config.toml" >/dev/null || fail "the latest release's arr setup refused the setup page's config"
run "setup page config, OLD runs" "$old" "$w" plain

echo "rollback: all checks passed"
