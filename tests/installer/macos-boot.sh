#!/bin/sh
# CI (Linux): install.sh's macOS "start at boot" logic, against stand-ins for launchd, sudo and
# the Mac-only tools, with the real arr binary. It checks the decisions and the sequence, not
# macOS itself (tests/installer/macos.sh does that on a real Mac):
#
#   - a login-item install, then --at-boot: the LaunchAgents go, launch daemons (UserName set,
#     same labels) come, and sudo is asked once;
#   - an unattended re-run keeps the daemons, never calls sudo, and still restarts Arr onto the
#     new binary by ending the user's own process (the stand-in refuses `launchctl kickstart` of
#     a system daemon without root, as launchd does);
#   - --no-at-boot unattended can't remove the daemons, and says so;
#   - the daily updater: its restart and its rollback (the "new release" reports the old version,
#     so the health check fails and the updater must put the old binary back), with Arr never
#     left stopped;
#   - uninstall.sh removes the daemons.
# A pty (python3) stands in for the terminal where the installer asks questions.
#
# Usage: sh tests/installer/macos-boot.sh path/to/arr      (from the repository root)
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
arr=${1:-}
[ -x "$arr" ] || fail "usage: $0 path/to/arr"
arr=$(cd "$(dirname "$arr")" && pwd)/$(basename "$arr")
repo=$(cd "$(dirname "$0")/../.." && pwd)
for t in python3 curl ps tar sha256sum; do command -v "$t" >/dev/null 2>&1 || fail "$t is needed"; done
freeport() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=$(freeport)
relport=$(freeport)

w=$(mktemp -d)
cleanup() {
	# Stand-in launchd jobs first, so nothing keeps restarting Arr.
	for j in "$w"/fake/state/job.*; do
		[ -e "$j" ] || continue
		rm -f "$j"
	done
	pkill -f "serve --config $w/" 2>/dev/null || true
	[ -z "${relpid:-}" ] || kill "$relpid" 2>/dev/null || true
	rm -rf "$w"
}
trap cleanup EXIT
export HOME="$w/home"
mkdir -p "$HOME" "$w/LaunchDaemons" "$w/fake/bin" "$w/fake/state" "$w/dist"
state="$HOME/Library/Application Support/arr"
bin="$HOME/.local/bin"
daemons="$w/LaunchDaemons"
agents="$HOME/Library/LaunchAgents"
calls="$w/fake/state/calls.log"
label=io.github.newtonharry.arr

# ---- stand-ins -------------------------------------------------------------------------
cat >"$w/fake/bin/uname" <<'EOF'
#!/bin/sh
case "$1" in -s) echo Darwin ;; -m) echo x86_64 ;; *) exec /usr/bin/uname "$@" ;; esac
EOF
# sudo: logs the call, becomes "root" for the command it runs (FAKE_ROOT=1), or refuses.
cat >"$w/fake/bin/sudo" <<'EOF'
#!/bin/sh
echo "sudo $*" >>"$FAKE_STATE/calls.log"
if [ "${FAKE_SUDO_DENY:-0}" = 1 ]; then echo "sudo: a password is required" >&2; exit 1; fi
while [ $# -gt 0 ]; do
	case "$1" in -v) exit 0 ;; -n | -E) shift ;; *) break ;; esac
done
[ $# -gt 0 ] || exit 0
FAKE_ROOT=1 exec "$@"
EOF
# install -o root -g wheel: those two don't exist here.
cat >"$w/fake/bin/install" <<'EOF'
#!/bin/sh
n=$#
while [ "$n" -gt 0 ]; do
	a=$1; shift; n=$((n - 1))
	case "$a" in -o | -g) shift; n=$((n - 1)) ;; *) set -- "$@" "$a" ;; esac
done
exec /usr/bin/install "$@"
EOF
cat >"$w/fake/bin/shlock" <<'EOF'
#!/bin/sh
f="" p=""
while [ $# -gt 0 ]; do case "$1" in -f) f=$2; shift ;; -p) p=$2; shift ;; esac; shift; done
if [ -e "$f" ] && kill -0 "$(cat "$f")" 2>/dev/null; then exit 1; fi
echo "$p" >"$f"
EOF
# launchctl: a tiny launchd. A job is "domain.label"; load/bootstrap start it (restarted when it
# ends, like KeepAlive), unload/bootout stop it. The system domain needs root, as in life.
cat >"$w/fake/bin/launchctl" <<'EOF'
#!/bin/sh
L=$FAKE_STATE
echo "root=${FAKE_ROOT:-0} launchctl $*" >>"$L/calls.log"
label_of() { sed -n 's#.*<key>Label</key><string>\([^<]*\)</string>.*#\1#p' "$1" | head -n1; }
strings_of() { awk '{ while (match($0, /<string>[^<]*<\/string>/)) {
	print substr($0, RSTART + 8, RLENGTH - 17); $0 = substr($0, RSTART + RLENGTH) } }' "$1"; }
need_root() { [ "${FAKE_ROOT:-0}" = 1 ] || { echo "Operation not permitted" >&2; exit 1; }; }
start_job() { # domain plist
	key="$1.$(label_of "$2")"
	[ ! -e "$L/job.$key" ] || { echo "$key already loaded" >&2; exit 1; }
	: >"$L/job.$key"
	case "$key" in *.io.github.newtonharry.arr) ;; *) return 0 ;; esac
	bin=$(strings_of "$2" | sed -n 2p) cfg=$(strings_of "$2" | sed -n 5p)
	# Its own session, like a job of launchd's: it outlives the terminal that loaded it.
	# With $L/slow present, Arr is the stand-in in $L/slow.d: same command line, but on SIGTERM it
	# stops answering at once and takes (cat slow) seconds to exit, like a busy Arr finishing a pass.
	setsid bash -c 'while [ -e "$1/job.$2" ]; do
		if [ -e "$1/slow" ]; then (cd "$1/slow.d" && exec -a "$3" python3 serve --config "$4") >>"$1/$2.log" 2>&1 </dev/null
		else "$3" serve --config "$4" >>"$1/$2.log" 2>&1 </dev/null; fi
		sleep 1; done' \
		bash "$L" "$key" "$bin" "$cfg" >/dev/null 2>&1 </dev/null &
	echo $! >"$L/job.$key"
}
stop_job() { # domain.label
	[ -e "$L/job.$1" ] || exit 1
	pid=$(cat "$L/job.$1"); rm -f "$L/job.$1"
	[ -z "$pid" ] || { pkill -P "$pid" 2>/dev/null || true; kill "$pid" 2>/dev/null || true; }
}
cmd=$1; shift
case "$cmd" in
load) while [ "${1#-}" != "$1" ]; do shift; done; start_job gui "$1" ;;
unload) while [ "${1#-}" != "$1" ]; do shift; done; stop_job "gui.$(label_of "$1")" ;;
bootstrap) need_root; start_job "$1" "$2" ;;
bootout) need_root; stop_job "$(echo "$1" | sed 's#/#.#')" ;;
kickstart)
	[ "$1" = -k ] && shift
	domain=${1%%/*} lab=${1##*/}
	[ "$domain" != system ] || need_root
	[ -e "$L/job.$domain.$lab" ] || { echo "Could not find service" >&2; exit 113; }
	pid=$(cat "$L/job.$domain.$lab"); pkill -P "$pid" 2>/dev/null || true ;;
*) ;;
esac
EOF
# chown root:wheel: the stand-in "root" is you, and Linux has no wheel.
printf '#!/bin/sh\nexit 0\n' >"$w/fake/bin/chown"
mkdir -p "$w/fake/state/slow.d"
cp "$repo/tests/installer/slowarr.py" "$w/fake/state/slow.d/serve"
chmod +x "$w/fake/bin"/*
PATH="$w/fake/bin:$PATH"
export PATH FAKE_STATE="$w/fake/state" ARR_LAUNCH_DAEMONS_DIR="$daemons"
sudo_calls() { grep -c '^sudo' "$calls" || true; }

# ---- a release to install, and one the updater can find ----------------------------------
target=x86_64-apple-darwin
mkdir -p "$w/dist/arr-$target"
cp "$arr" "$w/dist/arr-$target/arr"
tar -C "$w/dist" -czf "$w/dist/arr-$target.tar.gz" "arr-$target"
(cd "$w/dist" && sha256sum "arr-$target.tar.gz" >"arr-$target.tar.gz.sha256")
python3 "$repo/tests/installer/relserver.py" "$relport" "$w/dist" &
relpid=$!
sleep 1
# The shlock path is fixed in install.sh (a Mac has /usr/bin/shlock); point the copy at the stand-in.
sed "s#/usr/bin/shlock#$w/fake/bin/shlock#" "$repo/install.sh" >"$w/install.sh"
export ARR_DOWNLOAD_BASE="http://127.0.0.1:$relport/releases/download/v99.0.0" ARR_RELEASES_URL="http://127.0.0.1:$relport/releases"

# ---- helpers ---------------------------------------------------------------------------
retry() { # retry SECONDS COMMAND...
	n=$1; shift
	i=0
	until "$@" >/dev/null 2>&1; do i=$((i + 1)); [ "$i" -lt "$n" ] || return 1; sleep 1; done
}
healthy() { curl -fsS --max-time 3 "http://127.0.0.1:$port/api/v1/health" >/dev/null; }
# The user's own `arr serve` for this install (as install.sh finds it).
arr_pid() {
	ps -u "$(id -u)" -o pid= -o command= | awk -v want="$bin/arr serve --config $state/config.toml" \
		'{ p = $1; sub(/^[ ]*[0-9]+[ ]+/, ""); if ($0 == want) print p }' | head -n1
}
install_sh() { sh "$w/install.sh" "$@" </dev/null; }
no_agents() { for p in arr arr-tunnel arr-update; do [ ! -e "$agents/io.github.newtonharry.$p.plist" ] || fail "login item $p is still there ($1)"; done; }

"$arr" init --state-dir "$state" --listen "127.0.0.1:$port" >/dev/null

echo "== a login-item install (no flags, no terminal): no administrator, LaunchAgent"
: >"$calls"
install_sh --yes --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the login-item install failed"; }
[ -f "$agents/$label.plist" ] || fail "no LaunchAgent"
[ ! -e "$daemons/$label.plist" ] || fail "an unattended install made a daemon"
[ "$(sudo_calls)" = 0 ] || fail "an unattended install called sudo"
healthy || fail "Arr doesn't answer"
[ -n "$(arr_pid)" ] || fail "Arr isn't running"

echo "== --at-boot: daemons take over, asking sudo once"
: >"$calls"
old=$(arr_pid)
install_sh --yes --no-auto-update --at-boot >"$w/out" 2>&1 || { cat "$w/out"; fail "--at-boot failed"; }
no_agents "after --at-boot"
[ -f "$daemons/$label.plist" ] || fail "no daemon"
grep -q "<key>UserName</key><string>$(id -un)</string>" "$daemons/$label.plist" || fail "the daemon doesn't run as you"
grep -q "<key>HOME</key><string>$HOME</string>" "$daemons/$label.plist" || fail "the daemon doesn't get your HOME"
grep -q "<string>$bin/arr</string><string>serve</string><string>--config</string><string>$state/config.toml</string>" "$daemons/$label.plist" ||
	fail "the daemon doesn't run the same command"
grep -q '<key>KeepAlive</key><true/>' "$daemons/$label.plist" && grep -q '<key>RunAtLoad</key><true/>' "$daemons/$label.plist" || fail "no RunAtLoad/KeepAlive"
[ "$(grep -c '^sudo /bin/sh -s' "$calls")" = 1 ] || { cat "$calls"; fail "sudo should run one script"; }
grep -q 'root=1 launchctl bootstrap system' "$calls" || fail "the daemon wasn't bootstrapped as root"
retry 30 healthy || fail "Arr doesn't answer after --at-boot"
new=$(arr_pid)
[ -n "$new" ] && [ "$new" != "$old" ] || fail "Arr wasn't restarted into the daemon"
[ "$(stat -c %U "$state/config.toml")" = "$(id -un)" ] || fail "config.toml isn't yours"
[ -x "$bin/arr.previous" ] || fail "no arr.previous kept"

echo "== an unattended re-run keeps the daemons, asks nothing, restarts Arr by ending its process"
cksum "$daemons/$label.plist" >"$w/sum"
: >"$calls"
old=$(arr_pid)
install_sh --yes --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the unattended re-run failed"; }
[ "$(sudo_calls)" = 0 ] || { cat "$calls"; fail "an unattended re-run called sudo"; }
cksum "$daemons/$label.plist" | cmp -s - "$w/sum" || fail "an unattended re-run changed the daemon"
no_agents "after a re-run"
! grep -q 'bootout\|bootstrap\|unload\|load ' "$calls" || { cat "$calls"; fail "an unattended re-run touched launchd"; }
retry 30 healthy || fail "Arr doesn't answer after the re-run"
new=$(arr_pid)
[ -n "$new" ] && [ "$new" != "$old" ] || fail "the re-run didn't restart Arr onto the new binary"

echo "== --no-at-boot with nobody to ask leaves the daemons, and says so"
: >"$calls"
install_sh --yes --no-auto-update --no-at-boot >"$w/out" 2>&1 || { cat "$w/out"; fail "--no-at-boot failed"; }
grep -q 'taking that away needs an administrator' "$w/out" || { cat "$w/out"; fail "no explanation"; }
[ -f "$daemons/$label.plist" ] || fail "the daemon was removed without an administrator"
[ "$(sudo_calls)" = 0 ] || fail "--no-at-boot called sudo unattended"
no_agents "after --no-at-boot"
retry 30 healthy || fail "Arr doesn't answer"

echo "== a failed sudo on a fresh switch leaves Arr running as a login item"
# Take the daemon away by hand, back to a login item, to try the switch with sudo refused.
FAKE_ROOT=1 launchctl bootout "system/$label" 2>/dev/null || true
rm -f "$daemons/$label.plist"
pkill -f "serve --config $state/config.toml" 2>/dev/null || true
install_sh --yes --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "restoring the login item failed"; }
retry 30 healthy || fail "Arr isn't back as a login item"
: >"$calls"
if FAKE_SUDO_DENY=1 install_sh --yes --no-auto-update --at-boot >"$w/out" 2>&1; then
	cat "$w/out"; fail "--at-boot succeeded with sudo refused"
fi
cat "$w/out"
grep -q "couldn't set up start at boot" "$w/out" || fail "no explanation for the refused sudo"
[ ! -e "$daemons/$label.plist" ] || fail "a daemon was left behind"
retry 30 healthy || fail "Arr was left stopped by the refused sudo"
[ -f "$agents/$label.plist" ] || fail "the login item is gone"

echo "== a terminal: the question defaults to yes; --no-at-boot is honoured"
: >"$calls"
python3 "$repo/tests/installer/fakepty.py" sh "$w/install.sh" --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the terminal install failed"; }
grep -q 'Start Arr when this computer starts' "$w/out" || { cat "$w/out"; fail "the question wasn't asked"; }
[ -f "$daemons/$label.plist" ] || { cat "$w/out"; fail "the default answer should be yes (start at boot)"; }
no_agents "after answering the question"
[ "$(grep -c '^sudo /bin/sh -s' "$calls")" = 1 ] || fail "sudo should run one script"
retry 30 healthy || fail "Arr doesn't answer after the terminal install"
: >"$calls"
python3 "$repo/tests/installer/fakepty.py" sh "$w/install.sh" --no-auto-update --no-at-boot >"$w/out" 2>&1 || { cat "$w/out"; fail "the terminal --no-at-boot failed"; }
! grep -q 'Start Arr when this computer starts' "$w/out" || fail "--no-at-boot should not ask"
[ ! -e "$daemons/$label.plist" ] || { cat "$w/out"; fail "--no-at-boot at a terminal left the daemon"; }
[ -f "$agents/$label.plist" ] || fail "no login item after --no-at-boot"
retry 30 healthy || fail "Arr doesn't answer after going back to a login item"
install_sh --yes --no-auto-update --at-boot >"$w/out" 2>&1 || { cat "$w/out"; fail "--at-boot failed again"; }
[ -f "$daemons/$label.plist" ] || fail "no daemon"

if [ -n "${ARR_OLD_INSTALL_SH:-}" ]; then
	# Local only (the older script has to come from somewhere, for example
	# `git show origin/main:install.sh`): what an installer and an updater from before start-at-boot
	# do to an install that starts at boot. Arr must never be left stopped.
	echo "== an OLD install.sh against a start-at-boot install (ARR_OLD_INSTALL_SH)"
	sed "s#/usr/bin/shlock#$w/fake/bin/shlock#" "$ARR_OLD_INSTALL_SH" >"$w/old-install.sh"
	: >"$calls"
	sh "$w/old-install.sh" --yes --auto-update </dev/null >"$w/out" 2>&1 || { cat "$w/out"; echo "(the old installer failed: see above)"; }
	retry 30 healthy || fail "the old installer left Arr stopped"
	[ -f "$daemons/$label.plist" ] || fail "the old installer removed the daemon"
	echo "after the old installer: agent $([ -f "$agents/$label.plist" ] && echo present || echo absent), daemon present, Arr answers"
	echo "== an OLD updater against a start-at-boot install"
	cp "$bin/arr" "$w/arr.before"
	if ARR_RELEASES_URL="http://127.0.0.1:$relport/releases" sh "$bin/arr-update" >"$w/upd" 2>&1; then :; fi
	cat "$w/upd"
	cmp -s "$bin/arr" "$w/arr.before" || fail "the old updater left a different binary"
	retry 30 healthy || fail "the old updater left Arr stopped"
	echo "after the old updater: Arr answers"
	echo "== the new installer, unattended, puts it right (one copy of Arr, the daemon)"
	install_sh --yes --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the new installer failed after the old one"; }
	no_agents "after the new installer"
	retry 30 healthy || fail "Arr doesn't answer"
fi

echo "== the daily updater, started at boot: a daemon that restarts and rolls back without a password"
: >"$calls"
install_sh --yes --at-boot --auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "--auto-update failed"; }
[ -f "$daemons/io.github.newtonharry.arr-update.plist" ] || fail "no update daemon"
grep -q "<key>UserName</key><string>$(id -un)</string>" "$daemons/io.github.newtonharry.arr-update.plist" || fail "the update daemon doesn't run as you"
[ ! -e "$agents/io.github.newtonharry.arr-update.plist" ] || fail "an update login item is left next to the daemon"
[ -x "$bin/arr-update" ] || fail "no updater"
# "Update now" in the web app: it puts a file in update-queue, which launchd (QueueDirectories)
# watches for; unlike WatchPaths that doesn't run the job again when the updater deletes it.
grep -q "<key>QueueDirectories</key><array><string>$state/update-queue</string></array>" "$daemons/io.github.newtonharry.arr-update.plist" || fail "the update daemon doesn't watch the request folder"
[ -d "$state/update-queue" ] || fail "no request folder"
grep -q 'arr-update-trigger' "$bin/arr-update" || fail "the updater doesn't know the request file"
touch "$state/update-queue/requested"
# "v99.0.0" is served, but the binary in it is this one, so Arr answers as the old version:
# the updater must see the new one never became healthy and put the old binary back.
cp "$bin/arr" "$w/arr.before"
: >"$calls"
old=$(arr_pid)
if sh "$bin/arr-update" >"$w/upd" 2>&1; then cat "$w/upd"; fail "the updater called a version that never answered a success"; fi
cat "$w/upd"
grep -q 'updating v.* to v99.0.0' "$w/upd" || fail "the updater didn't try the update"
[ -z "$(ls "$state/update-queue")" ] || fail "the updater left the request in the folder (launchd would run it again)"
grep -q '"state":"failed"' "$state/update-status.json" || { cat "$state/update-status.json"; fail "a rolled-back update isn't reported as failed"; }
grep -q "v99.0.0 didn't start; going back" "$w/upd" || fail "the updater didn't roll back"
! grep -q "didn't start either" "$w/upd" || fail "the old version didn't come back"
[ "$(sudo_calls)" = 0 ] || fail "the updater called sudo"
! grep -q 'kickstart.*system' "$calls" || fail "the updater tried to kickstart a system daemon"
cmp -s "$bin/arr" "$w/arr.before" || fail "the old binary wasn't put back"
retry 30 healthy || fail "Arr was left stopped by the rollback"
new=$(arr_pid)
[ -n "$new" ] && [ "$new" != "$old" ] || fail "the updater never restarted Arr"

echo "== a busy Arr: SIGTERM returns long before it exits; the restarts must wait for it"
# The stand-in answers as the old version, then stops answering on SIGTERM and exits 33 s later
# (longer than the 30 s health check). Without the wait, the health check would end before
# launchd has even started the replacement.
"$arr" --version | awk '{print $2}' >"$w/fake/state/version"
echo 33 >"$w/fake/state/slow"
kill "$(arr_pid)"
retry 60 healthy || fail "the stand-in Arr didn't start"
# The updater: the "new" release reports itself as v99.0.0 (the stand-in reads its version at start).
echo 99.0.0 >"$w/fake/state/version"
old=$(arr_pid)
sh "$bin/arr-update" >"$w/upd" 2>&1 || { cat "$w/upd"; fail "an update of a busy Arr was rolled back"; }
grep -q 'now running v99.0.0' "$w/upd" || { cat "$w/upd"; fail "the updater didn't report the new version"; }
new=$(arr_pid)
[ -n "$new" ] && [ "$new" != "$old" ] || fail "the updater didn't restart the busy Arr"
# The installer's own restart.
old=$new
install_sh --yes --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "a rerun on a busy Arr failed"; }
new=$(arr_pid)
[ -n "$new" ] && [ "$new" != "$old" ] || fail "the rerun didn't restart the busy Arr"
rm -f "$w/fake/state/slow"
kill "$(arr_pid)"
retry 60 healthy || fail "Arr didn't come back after the busy test"

echo "== unattended, without an administrator, the update job falls back to a login item"
FAKE_ROOT=1 launchctl bootout system/io.github.newtonharry.arr-update 2>/dev/null || true
rm -f "$bin/arr-update" "$daemons/io.github.newtonharry.arr-update.plist"
: >"$calls"
FAKE_SUDO_DENY=1 install_sh --yes --auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the unattended --auto-update failed"; }
! grep -q '^sudo' "$calls" || { cat "$calls"; fail "an unattended run called sudo"; }
[ -x "$bin/arr-update" ] || fail "no updater"
[ -f "$agents/io.github.newtonharry.arr-update.plist" ] || fail "no update login item"
[ ! -e "$daemons/io.github.newtonharry.arr-update.plist" ] || fail "an unattended run made a daemon"
[ -f "$daemons/$label.plist" ] && [ ! -e "$agents/$label.plist" ] || fail "an unattended run changed Arr's own service"
retry 30 healthy || fail "Arr doesn't answer"

echo "== uninstall.sh removes the daemons"
: >"$calls"
sh "$repo/uninstall.sh" --yes --arr --delete-data >"$w/out" 2>&1 || { cat "$w/out"; fail "uninstall.sh failed"; }
cat "$w/out"
for p in arr arr-tunnel arr-update; do
	[ ! -e "$daemons/io.github.newtonharry.$p.plist" ] || fail "daemon $p is still there"
done
no_agents "after uninstall"
[ ! -e "$agents/io.github.newtonharry.arr-update.plist" ] || fail "the update login item is still there"
[ ! -e "$state" ] || fail "Arr's data is still there"
[ ! -e "$bin/arr" ] || fail "the arr program is still there"
grep -q 'root=1 launchctl bootout system/io.github.newtonharry.arr$' "$calls" || fail "the daemon wasn't stopped"
echo "macos boot (stand-ins): all checks passed"
