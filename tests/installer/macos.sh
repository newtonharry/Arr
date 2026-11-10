#!/bin/sh
# CI: on macOS, the real "Install Plex for me" (arr plex-install), then install.sh with remote
# access (real ngrok agent, made-up account: the public check must fail pointing at the
# tunnel's log), a rerun, and uninstall.sh removing Arr and Plex while keeping media.
# (A real Plex sign-in, ngrok account and Google sign-in can't be exercised in CI.)
#
# Run from the repo root after `cargo build --release`, as a normal (non-root) user.
set -eu
fail() { echo "FAIL: $*"; exit 1; }
case "$(uname -m)" in arm64) target=aarch64-apple-darwin ;; *) target=x86_64-apple-darwin ;; esac

echo "== Install Plex for me"
./target/release/arr plex-install >plex.out 2>&1 || { cat plex.out; fail "plex-install failed"; }
cat plex.out
grep -q '^step: Installing Plex Media Server' plex.out || fail "no install step reported"
app="/Applications/Plex Media Server.app"
[ -d "$app" ] || app="$HOME/Applications/Plex Media Server.app"
[ -d "$app" ] || fail "the Plex app isn't in Applications"
[ -f "$HOME/Library/LaunchAgents/io.github.newtonharry.plex.plist" ] || fail "Plex won't start at log-in"
for d in Movies "TV Shows"; do [ -d "$HOME/Media/$d" ] || fail "no ~/Media/$d"; done
curl -fsS -H 'Accept: application/json' http://127.0.0.1:32400/identity | tee identity.json
grep -q '"claimed":false' identity.json || fail "Plex isn't answering as a new, unlinked server"
touch "$HOME/Media/Movies/keep.mkv"

echo "== install.sh with remote access"
dist=$(mktemp -d)
mkdir -p "$dist/arr-$target"
cp target/release/arr README.md "$dist/arr-$target/"
tar -C "$dist" -czf "$dist/arr-$target.tar.gz" "arr-$target"
(cd "$dist" && shasum -a 256 "arr-$target.tar.gz" >"arr-$target.tar.gz.sha256")
python3 -m http.server 8997 --bind 127.0.0.1 --directory "$dist" >/dev/null 2>&1 &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
sleep 2
state="$HOME/Library/Application Support/arr"
./target/release/arr init --state-dir "$state" >/dev/null
export ARR_DOWNLOAD_BASE=http://127.0.0.1:8997 ARR_REMOTE_CHECK_SECONDS=20
export ARR_NGROK_TOKEN=2abcDEF_ciNotARealToken ARR_GOOGLE_CLIENT_SECRET=GOCSPX-ci_secret
set -- --yes --no-auto-update --ngrok-domain https://Arr-CI-Test.ngrok-free.app/ \
	--google-client-id 123-ci.apps.googleusercontent.com --allow-email CI@Example.com
if sh install.sh "$@" >install.out 2>&1; then cat install.out; fail "the public check passed with a made-up ngrok account"; fi
cat install.out
grep -q "doesn't reach Arr yet" install.out || fail "no public-check failure"
grep -qF "$state/tunnel.log" install.out || fail "the failure doesn't point at the tunnel's log"
grep -qF 'public_url = "https://arr-ci-test.ngrok-free.app"' "$state/config.toml" || fail "config lacks public_url"
grep -q GOCSPX "$state/config.toml" && fail "the client secret is in the config"
[ "$(stat -f %Lp "$state/secrets/google_client_secret")" = 600 ] || fail "client secret isn't private"
for p in arr arr-tunnel; do
	[ -f "$HOME/Library/LaunchAgents/io.github.newtonharry.$p.plist" ] || fail "no $p login agent"
done
curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "arr doesn't answer"
[ -s "$state/tunnel.log" ] || fail "ngrok wrote no log"
echo "tunnel log:"; tail -n 3 "$state/tunnel.log"
before=$(cat "$state/config.toml")

# After every rerun Arr runs and answers, and its config is still the runner's.
still_running() {
	launchctl list io.github.newtonharry.arr >/dev/null || fail "arr isn't loaded after $1"
	curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "arr doesn't answer after $1"
	[ "$(stat -f %Su "$state/config.toml")" = "$(id -un)" ] || fail "config.toml isn't $(id -un)'s after $1"
}
still_running "the install"
# Arr's process: a rerun that applies something restarts it, a refused one doesn't.
arr_pid() { launchctl list io.github.newtonharry.arr | sed -n 's/.*"PID" = \([0-9]*\);.*/\1/p'; }
# Remote access is set up (with the made-up ngrok account), so a rerun that otherwise works
# ends at the public check: it must fail there, after Arr is running, and nowhere else.
ends_at_public_check() { # ends_at_public_check OUTPUT_FILE WHAT
	! grep -q 'app/link' "$1" || { cat "$1"; fail "$2 printed a sign-in link without a person at the terminal"; }
	grep -q '^Arr is running\. Open it at:' "$1" && grep -q '^  this computer: http://localhost:8787$' "$1" &&
		grep -q '^  any device:    https://arr-ci-test.ngrok-free.app   (sign in with Google)$' "$1" &&
		grep -q "doesn't reach Arr yet" "$1" ||
		{ cat "$1"; fail "$2 failed before the public check"; }
}

echo "== a rerun keeps remote access"
if sh install.sh "$@" >rerun.out 2>&1; then cat rerun.out; fail "the public check passed on a rerun"; fi
ends_at_public_check rerun.out "a rerun"
grep -q 'already has remote access; keeping it' rerun.out || { cat rerun.out; fail "rerun didn't keep remote access"; }
[ "$(cat "$state/config.toml")" = "$before" ] || fail "a rerun changed the config"
[ -x "$HOME/.local/bin/arr.previous" ] || fail "the rerun didn't keep the previous binary"
still_running "a rerun"

echo "== a rerun with a first-setup-only option is refused, and changes nothing"
pid=$(arr_pid)
[ -n "$pid" ] || fail "launchctl shows no PID for Arr"
if sh install.sh --yes --no-auto-update --movies /elsewhere >refused.out 2>&1; then
	cat refused.out; fail "--movies on an existing install was accepted"
fi
cat refused.out
grep -q -- '--movies only applies to the first setup' refused.out || fail "the refusal doesn't say why"
[ "$(cat "$state/config.toml")" = "$before" ] || fail "a refused rerun changed the config"
[ "$(arr_pid)" = "$pid" ] || fail "a refused rerun restarted Arr"
still_running "a refused rerun"

echo "== a rerun applies a setting it's given"
pid=$(arr_pid)
if sh install.sh --yes --no-auto-update --no-error-reports >setting.out 2>&1; then
	cat setting.out; fail "the public check passed on a rerun"
fi
ends_at_public_check setting.out "a rerun with --no-error-reports"
grep -A1 '^\[reporting\]$' "$state/config.toml" | grep -q '^enabled = false$' || { cat setting.out; fail "--no-error-reports wasn't applied"; }
[ "$(arr_pid)" != "$pid" ] || fail "a rerun that changed a setting didn't restart Arr"
still_running "a rerun with --no-error-reports"

echo "== --at-boot: launch daemons that run as you, started before anyone logs in"
label=io.github.newtonharry
daemons=/Library/LaunchDaemons
# A sudo that records its use and refuses: from here on, nothing may need a password.
shim=$(mktemp -d)
printf '#!/bin/sh\necho "$*" >>"%s/sudo.called"\nexit 1\n' "$shim" >"$shim/sudo"
chmod +x "$shim/sudo"
daemon_pid() { sudo launchctl print "system/$label.arr" | sed -n 's/^[[:space:]]*pid = \([0-9][0-9]*\)$/\1/p'; }
# Passwordless sudo on the runner stands in for the one password a person types.
if sh install.sh "$@" --at-boot >boot.out 2>&1; then cat boot.out; fail "the public check passed on --at-boot"; fi
ends_at_public_check boot.out "--at-boot"
for p in arr arr-tunnel; do
	[ -f "$daemons/$label.$p.plist" ] || { cat boot.out; fail "no $p daemon"; }
	[ "$(stat -f %Su "$daemons/$label.$p.plist")" = root ] || fail "the $p daemon isn't root's file"
	[ ! -e "$HOME/Library/LaunchAgents/$label.$p.plist" ] || fail "the $p login item is still there next to its daemon"
	sudo plutil -lint "$daemons/$label.$p.plist" >/dev/null || fail "the $p daemon is not a valid plist"
done
grep -q "<key>UserName</key><string>$(id -un)</string>" "$daemons/$label.arr.plist" || fail "the daemon doesn't run as you"
pid=$(daemon_pid)
[ -n "$pid" ] || { sudo launchctl print "system/$label.arr"; fail "launchd shows no running Arr daemon"; }
[ "$(ps -o user= -p "$pid" | tr -d ' ')" = "$(id -un)" ] || fail "the Arr daemon doesn't run as $(id -un)"
curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "the daemon doesn't answer"
[ "$(stat -f %Su "$state/arr.log")" = "$(id -un)" ] || fail "arr.log isn't yours"
[ "$(stat -f %Su "$state/config.toml")" = "$(id -un)" ] || fail "config.toml isn't yours"
[ ! -e "$shim/sudo.called" ] || fail "the sudo shim was called"

echo "== a rerun without a terminal asks for no password and restarts Arr by ending its process"
if PATH="$shim:$PATH" sh install.sh --yes --no-auto-update >unattended.out 2>&1; then cat unattended.out; fail "the public check passed on a rerun"; fi
ends_at_public_check unattended.out "an unattended rerun"
[ ! -e "$shim/sudo.called" ] || { cat "$shim/sudo.called"; fail "an unattended rerun used sudo"; }
new=$(daemon_pid)
[ -n "$new" ] && [ "$new" != "$pid" ] || fail "an unattended rerun didn't restart the daemon (pid $pid -> $new)"
curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "arr doesn't answer after the unattended rerun"
[ -f "$daemons/$label.arr.plist" ] && [ ! -e "$HOME/Library/LaunchAgents/$label.arr.plist" ] || fail "an unattended rerun changed the kind of install"

echo "== the updater at boot: restarts Arr, and rolls back, with no password"
# "v99.0.0" is served, but holds this build: Arr answers as the old version, so the updater
# must put the old binary back and start it again.
python3 tests/installer/relserver.py 8996 "$dist" >/dev/null 2>&1 &
relserver=$!
trap 'kill $server $relserver 2>/dev/null || true' EXIT
sleep 1
if sh install.sh "$@" --at-boot --auto-update >update-install.out 2>&1; then cat update-install.out; fail "the public check passed"; fi
[ -f "$daemons/$label.arr-update.plist" ] || { cat update-install.out; fail "no update daemon"; }
[ ! -e "$HOME/Library/LaunchAgents/$label.arr-update.plist" ] || fail "an update login item next to the update daemon"
cp "$HOME/.local/bin/arr" arr.before
pid=$(daemon_pid)
if ARR_RELEASES_URL=http://127.0.0.1:8996/releases PATH="$shim:$PATH" sh "$HOME/.local/bin/arr-update" >update.out 2>&1; then
	cat update.out; fail "the updater called a version that never answered a success"
fi
cat update.out
grep -q "v99.0.0 didn't start; going back" update.out || fail "the updater didn't roll back"
! grep -q "didn't start either" update.out || fail "the old version didn't come back"
cmp -s "$HOME/.local/bin/arr" arr.before || fail "the old binary wasn't put back"
[ ! -e "$shim/sudo.called" ] || { cat "$shim/sudo.called"; fail "the updater used sudo"; }
curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "Arr was left stopped by the rollback"
new=$(daemon_pid)
[ -n "$new" ] && [ "$new" != "$pid" ] || fail "the updater never restarted the daemon"

echo "== running as root on a Mac is refused"
if sudo sh install.sh --yes >root.out 2>&1; then cat root.out; fail "install.sh ran as root on a Mac"; fi
grep -q 'without sudo' root.out || { cat root.out; fail "the refusal doesn't say to run it without sudo"; }

echo "== uninstall Arr and Plex, keeping media"
sh uninstall.sh --yes --all --delete-data
[ ! -e "$app" ] || fail "the Plex app is still there"
[ ! -e "$HOME/Library/LaunchAgents/io.github.newtonharry.plex.plist" ] || fail "Plex's login agent is still there"
for p in arr arr-tunnel arr-update; do
	[ ! -e "$HOME/Library/LaunchAgents/io.github.newtonharry.$p.plist" ] || fail "$p login agent is still there"
	[ ! -e "/Library/LaunchDaemons/io.github.newtonharry.$p.plist" ] || fail "$p launch daemon is still there"
done
! sudo launchctl print system/io.github.newtonharry.arr >/dev/null 2>&1 || fail "the Arr daemon is still loaded"
[ ! -e "$state" ] || fail "Arr's data is still there"
[ ! -e "$HOME/.local/bin/arr" ] || fail "the arr program is still there"
[ -f "$HOME/Media/Movies/keep.mkv" ] || fail "media was deleted"
echo "macos installer: all checks passed"
