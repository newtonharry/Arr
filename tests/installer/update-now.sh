#!/bin/sh
# CI (Linux): what the web app's "Update now" relies on, with stand-ins for systemd (so this
# starts and stops nothing real) and for GitHub's release pages. A per-user install.sh with
# automatic updates must:
#
#   - add an arr-update.path unit that watches the state folder's request file and starts
#     arr-update.service, and enable it next to the timer;
#   - write an updater that deletes the request file first thing (so a run that fails can't
#     start itself again), and says how it went in update-status.json: up_to_date, failed;
#   - never follow a link left at the status file's name (the system updater runs as root, in a
#     folder another account owns).
#
# The real systemd path unit starting the real service runs in linux.sh. The macOS and Windows
# versions run in the macos and windows jobs.
#
# Usage: sh tests/installer/update-now.sh path/to/arr      (from the repository root)
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
arr=${1:-}
[ -x "$arr" ] || fail "usage: $0 path/to/arr"
arr=$(cd "$(dirname "$arr")" && pwd)/$(basename "$arr")
repo=$(cd "$(dirname "$0")/../.." && pwd)
case "$(uname -m)" in x86_64 | amd64) target=x86_64-unknown-linux-gnu ;; aarch64 | arm64) target=aarch64-unknown-linux-gnu ;; *) fail "no target for $(uname -m)" ;; esac
freeport() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=$(freeport)
relport=$(freeport)
oldport=$(freeport)

w=$(mktemp -d)
trap 'pkill -f "serve --config $w/" 2>/dev/null || true; [ -z "${relpid:-}" ] || kill "$relpid" 2>/dev/null || true; [ -z "${oldpid:-}" ] || kill "$oldpid" 2>/dev/null || true; rm -rf "$w"' EXIT
export HOME="$w/home"
unset XDG_DATA_HOME XDG_CONFIG_HOME
mkdir -p "$HOME" "$w/fake/bin" "$w/dist" "$w/old"
state="$HOME/.local/share/arr"
bin="$HOME/.local/bin"
units="$HOME/.config/systemd/user"
calls="$w/fake/calls.log"
export FAKE_DIR="$w/fake"

cat >"$w/fake/bin/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >>"$FAKE_DIR/calls.log"
EOF
cat >"$w/fake/bin/loginctl" <<'EOF'
#!/bin/sh
case "$*" in *show-user*) echo "Linger=yes" ;; esac
EOF
chmod +x "$w/fake/bin"/*
PATH="$w/fake/bin:$PATH"
export PATH

mkdir -p "$w/dist/arr-$target"
cp "$arr" "$w/dist/arr-$target/arr"
tar -C "$w/dist" -czf "$w/dist/arr-$target.tar.gz" "arr-$target"
(cd "$w/dist" && sha256sum "arr-$target.tar.gz" >"arr-$target.tar.gz.sha256")
python3 -m http.server "$relport" --bind 127.0.0.1 --directory "$w/dist" >/dev/null 2>&1 &
relpid=$!
# GitHub's release pages, with a newest release older than this one: nothing to install.
python3 "$repo/tests/installer/relserver.py" "$oldport" "$w/old" 0.0.1 &
oldpid=$!
export ARR_DOWNLOAD_BASE="http://127.0.0.1:$relport"
sleep 1

# The service manager is a stand-in, so Arr is started here, once, for the health check.
"$arr" init --state-dir "$state" --listen "127.0.0.1:$port" >/dev/null
"$arr" serve --config "$state/config.toml" >"$w/serve.log" 2>&1 &
i=0
until curl -fsS "http://127.0.0.1:$port/api/v1/health" >/dev/null 2>&1; do
	i=$((i + 1)); [ "$i" -lt 30 ] || { cat "$w/serve.log"; fail "Arr didn't start"; }
	sleep 1
done

echo "== the installer adds the watcher and an updater that knows the request file"
: >"$calls"
sh "$repo/install.sh" --yes --auto-update --no-at-boot >"$w/out" 2>&1 </dev/null || { cat "$w/out"; fail "install.sh --auto-update failed"; }
[ -x "$bin/arr-update" ] || { cat "$w/out"; fail "no updater"; }
unit="$units/arr-update.path"
[ -f "$unit" ] || fail "no arr-update.path"
grep -qx "PathExists=$state/update-requested" "$unit" || { cat "$unit"; fail "the path unit watches the wrong file"; }
grep -qx 'Unit=arr-update.service' "$unit" || fail "the path unit starts the wrong service"
grep -qx 'WantedBy=paths.target' "$unit" || fail "the path unit isn't wanted by paths.target"
grep -q 'systemctl --user enable --now arr-update.path' "$calls" || { cat "$calls"; fail "the path unit wasn't enabled"; }
grep -q 'systemctl --user enable --now arr-update.timer' "$calls" || fail "the timer wasn't enabled"
grep -q 'arr-update-trigger' "$bin/arr-update" || fail "the updater doesn't know the request file"
if command -v systemd-analyze >/dev/null 2>&1; then
	systemd-analyze verify --user "$unit" "$units/arr-update.service" 2>"$w/verify" || { cat "$w/verify"; fail "systemd finds a problem with the path unit"; }
fi

run_updater() { # run_updater RELEASES_URL
	ARR_RELEASES_URL="$1" sh "$bin/arr-update" >"$w/upd" 2>&1
}
status_is() { # status_is STATE
	python3 - "$state/update-status.json" "$1" <<'PY'
import json, sys
s = json.load(open(sys.argv[1]))
assert s["state"] == sys.argv[2], s
assert s["at"].endswith("Z") and set(s) == {"state", "from", "to", "at"}, s
PY
}

echo "== a request: the file goes first, and the status says it is up to date"
touch "$state/update-requested"
run_updater "http://127.0.0.1:$oldport/releases" || { cat "$w/upd"; fail "the updater failed with nothing to do"; }
[ ! -e "$state/update-requested" ] || fail "the updater left the request file"
status_is up_to_date || fail "the status isn't up_to_date"
grep -q '"to":"0.0.1"' "$state/update-status.json" || fail "the status doesn't name the latest version"
[ "$(stat -c %a "$state/update-status.json")" = 644 ] || fail "Arr (another account) couldn't read the status"

echo "== a failing run: the file still goes first, the status says failed, and nothing loops"
touch "$state/update-requested"
if run_updater "http://127.0.0.1:1/releases"; then cat "$w/upd"; fail "the updater passed with no release page"; fi
[ ! -e "$state/update-requested" ] || fail "a failed run left the request file (it would run again)"
status_is failed || { cat "$state/update-status.json"; fail "the status isn't failed"; }

echo "== a link left at the status file's name is replaced, never followed"
echo untouched >"$w/victim"
rm -f "$state/update-status.json"
ln -s "$w/victim" "$state/update-status.json"
run_updater "http://127.0.0.1:$oldport/releases" || { cat "$w/upd"; fail "the updater failed with a link in the way"; }
[ "$(cat "$w/victim")" = untouched ] || fail "the updater wrote through a link"
[ ! -L "$state/update-status.json" ] || fail "the link is still there"
status_is up_to_date || fail "no status after replacing the link"

echo "== the updater only removes the request's own name: it never follows or walks links in the state folder"
mkdir -p "$w/outside"
echo keep >"$w/outside/precious"
echo keep >"$w/victim2"
ln -s "$w/outside" "$state/update-queue"
ln -s "$w/victim2" "$state/update-requested"
run_updater "http://127.0.0.1:$oldport/releases" || { cat "$w/upd"; fail "the updater failed with links in the way"; }
[ "$(cat "$w/outside/precious")" = keep ] || fail "the updater reached through a link to a folder"
[ "$(cat "$w/victim2")" = keep ] || fail "the updater reached through the request's link"
[ ! -e "$state/update-requested" ] && [ ! -L "$state/update-requested" ] || fail "a link left as the request would start the updater again and again"
rm -f "$state/update-queue"

echo "== as root, everything in the state folder is done as the account Arr runs as (baked in), never by root"
# Stand-ins: id says root, and setpriv records who it was asked to become, then runs the command.
mkdir -p "$w/root/bin"
cat >"$w/root/bin/id" <<'EOF'
#!/bin/sh
if [ "$1" = -u ]; then echo 0; else exec /usr/bin/id "$@"; fi
EOF
cat >"$w/root/bin/setpriv" <<'EOF'
#!/bin/sh
echo "$1" >>"$FAKE_DIR/setpriv.log"
while [ "$1" != sh ]; do shift; done
exec "$@"
EOF
chmod +x "$w/root/bin"/*
grep -q "^OWNER='$(id -un)'$" "$bin/arr-update" || fail "the updater doesn't carry the account Arr runs as"
touch "$state/update-requested"
rm -f "$state/update-status.json" "$FAKE_DIR/setpriv.log"
PATH="$w/root/bin:$PATH" run_updater "http://127.0.0.1:$oldport/releases" || { cat "$w/upd"; fail "the updater failed as 'root'"; }
[ "$(sort -u "$FAKE_DIR/setpriv.log")" = "--reuid=$(id -un)" ] || { cat "$FAKE_DIR/setpriv.log"; fail "root wrote in the state folder without becoming the account Arr runs as"; }
[ "$(grep -c . "$FAKE_DIR/setpriv.log")" -ge 2 ] || fail "the request removal and the status were not both done as that account"
[ ! -e "$state/update-requested" ] || fail "the request wasn't removed"
status_is up_to_date || fail "no status after the run as 'root'"
# With neither tool, root does nothing there (and says nothing): no status.
rm -f "$state/update-status.json"
mkdir -p "$w/notools"
for t in sh dirname mktemp cat chmod mv rm date printf curl sed awk tr cut grep ls head sort env flock uname mkdir stat; do
	p=$(command -v "$t" 2>/dev/null) && [ -x "$p" ] && ln -sf "$p" "$w/notools/$t"
done
cp "$w/root/bin/id" "$w/notools/id"
if PATH="$w/notools" ARR_RELEASES_URL="http://127.0.0.1:$oldport/releases" /bin/sh "$bin/arr-update" >"$w/upd" 2>&1; then :; fi
[ ! -e "$state/update-status.json" ] || fail "root wrote the status itself with no way to become the account"

echo "== no request, no leftover: a daily run leaves no request file behind"
run_updater "http://127.0.0.1:$oldport/releases" || fail "the daily run failed"
[ ! -e "$state/update-requested" ] || fail "a daily run made a request file"

echo "== turning automatic updates off removes the watcher too"
sh "$repo/install.sh" --yes --no-auto-update --no-at-boot >"$w/out" 2>&1 </dev/null || { cat "$w/out"; fail "install.sh --no-auto-update failed"; }
for f in arr-update.path arr-update.timer arr-update.service; do
	[ ! -e "$units/$f" ] || fail "$f is still installed"
done
[ ! -e "$bin/arr-update" ] || fail "the updater is still installed"

echo "update-now: all checks passed"
