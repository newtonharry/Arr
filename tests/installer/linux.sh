#!/bin/sh
# CI: install.sh and uninstall.sh on Linux, system-wide with sudo, the way a user runs them,
# against a release built in this job. Remote access uses the real ngrok agent with a
# made-up account: everything up to the public check runs for real, and that check must
# fail pointing at the tunnel's log. (A real ngrok account and Google sign-in can't be
# exercised in CI.)
#
# Run from the repo root after `cargo build --release`; needs passwordless sudo and systemd.
set -eu
fail() { echo "FAIL: $*"; exit 1; }
target=x86_64-unknown-linux-gnu
dist=$(mktemp -d)
mkdir -p "$dist/arr-$target"
cp target/release/arr README.md "$dist/arr-$target/"
tar -C "$dist" -czf "$dist/arr-$target.tar.gz" "arr-$target"
(cd "$dist" && sha256sum "arr-$target.tar.gz" >"arr-$target.tar.gz.sha256")
python3 -m http.server 8997 --bind 127.0.0.1 --directory "$dist" >/dev/null 2>&1 &
server=$!
trap 'kill $server 2>/dev/null || true' EXIT
sleep 2

state=/var/lib/arr
sudo ./target/release/arr init --state-dir "$state" >/dev/null
export ARR_DOWNLOAD_BASE=http://127.0.0.1:8997 ARR_REMOTE_CHECK_SECONDS=20
export ARR_NGROK_TOKEN=2abcDEF_ciNotARealToken ARR_GOOGLE_CLIENT_SECRET=GOCSPX-ci_secret
remote="--yes --no-auto-update --ngrok-domain https://Arr-CI-Test.ngrok-free.app/ --google-client-id 123-ci.apps.googleusercontent.com --allow-email CI@Example.com --allow-email other@example.com"

echo "== install with remote access"
# shellcheck disable=SC2086
if sudo -E sh install.sh $remote >install.out 2>&1; then cat install.out; fail "the public check passed with a made-up ngrok account"; fi
cat install.out
grep -q "doesn't reach Arr yet" install.out || fail "no public-check failure"
grep -q '^Arr is running\. Open it at:' install.out || fail "the install doesn't say where to open Arr"
grep -q '^  any device:    https://arr-ci-test.ngrok-free.app   (sign in with Google)$' install.out || fail "the install doesn't show the public address"
grep -q 'app/link' install.out && fail "a non-interactive install printed a sign-in link"
grep -q 'journalctl -u arr-tunnel' install.out || fail "the failure doesn't point at the tunnel's log"

config=$(sudo cat "$state/config.toml")
for want in 'public_url = "https://arr-ci-test.ngrok-free.app"' \
	'allowed_emails = ["ci@example.com", "other@example.com"]' \
	'allowed_hosts = ["arr-ci-test.ngrok-free.app", "localhost", "127.0.0.1"]' \
	'google_client_secret_file = "secrets/google_client_secret"'; do
	printf '%s' "$config" | grep -qF "$want" || fail "config lacks $want"
done
printf '%s' "$config" | grep -q GOCSPX && fail "the client secret is in the config"
[ "$(sudo cat "$state/secrets/google_client_secret")" = GOCSPX-ci_secret ] || fail "client secret file"
[ "$(sudo stat -c %a "$state/secrets/google_client_secret")" = 600 ] || fail "client secret isn't private"
sudo grep -q 'web_addr: false' "$state/ngrok.yml" || fail "ngrok.yml keeps the inspection page"
# The service runs as the user who ran sudo, who now owns Arr's folder.
/usr/local/bin/arr check --config "$state/config.toml" | grep -q '"mcp_enabled": true' || fail "arr check"
for u in arr arr-tunnel; do
	systemctl is-enabled "$u" >/dev/null || fail "$u isn't enabled (it wouldn't start at boot)"
done
systemctl is-active arr >/dev/null || fail "arr isn't running"
curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "arr doesn't answer"
sleep 3
sudo journalctl -u arr-tunnel --no-pager | grep -q 'ERR_NGROK\|authentication failed' ||
	fail "ngrok didn't run (no error from ngrok's servers in its log)"
echo "tunnel log:"; sudo journalctl -u arr-tunnel --no-pager | tail -n 3

# After every rerun Arr runs and answers, as the user who ran sudo, who still owns its config.
still_running() {
	systemctl is-active arr >/dev/null || fail "arr isn't running after $1"
	curl -fsS http://127.0.0.1:8787/api/v1/health >/dev/null || fail "arr doesn't answer after $1"
	[ "$(sudo stat -c %U "$state/config.toml")" = "$(id -un)" ] || fail "config.toml isn't $(id -un)'s after $1"
}
still_running "the install"
# Arr's process: a rerun that applies something restarts it, a refused one doesn't.
arr_pid() { systemctl show -p MainPID --value arr; }
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
# shellcheck disable=SC2086
if sudo -E sh install.sh $remote >rerun.out 2>&1; then cat rerun.out; fail "the public check passed on a rerun"; fi
ends_at_public_check rerun.out "a rerun"
grep -q 'already has remote access; keeping it' rerun.out || { cat rerun.out; fail "rerun didn't keep remote access"; }
[ "$(sudo cat "$state/config.toml")" = "$config" ] || fail "a rerun changed the config"
sudo test -x /usr/local/bin/arr.previous || fail "the rerun didn't keep the previous binary"
still_running "a rerun"

echo "== a rerun with a first-setup-only option is refused, and changes nothing"
pid=$(arr_pid)
[ "${pid:-0}" != 0 ] || fail "systemd shows no PID for Arr"
if sudo -E sh install.sh --yes --no-auto-update --movies /elsewhere >refused.out 2>&1; then
	cat refused.out; fail "--movies on an existing install was accepted"
fi
cat refused.out
grep -q -- '--movies only applies to the first setup' refused.out || fail "the refusal doesn't say why"
[ "$(sudo cat "$state/config.toml")" = "$config" ] || fail "a refused rerun changed the config"
[ "$(arr_pid)" = "$pid" ] || fail "a refused rerun restarted Arr"
still_running "a refused rerun"

echo "== a rerun applies a setting it's given"
pid=$(arr_pid)
if sudo -E sh install.sh --yes --no-auto-update --no-error-reports >setting.out 2>&1; then
	cat setting.out; fail "the public check passed on a rerun"
fi
ends_at_public_check setting.out "a rerun with --no-error-reports"
sudo grep -A1 '^\[reporting\]$' "$state/config.toml" | grep -q '^enabled = false$' || { cat setting.out; fail "--no-error-reports wasn't applied"; }
[ "$(arr_pid)" != "$pid" ] || fail "a rerun that changed a setting didn't restart Arr"
still_running "a rerun with --no-error-reports"

echo "== update now: systemd starts the updater when the web app's request file appears"
# A stand-in for GitHub's release pages whose newest release is older than this one, so the
# updater has nothing to install; the service manager passes it the address.
relsrv_dir=$(mktemp -d)
python3 tests/installer/relserver.py 8995 "$relsrv_dir" 0.0.1 &
relsrv=$!
trap 'kill $server $relsrv 2>/dev/null || true; sudo systemctl unset-environment ARR_RELEASES_URL 2>/dev/null || true' EXIT
sleep 1
sudo systemctl set-environment ARR_RELEASES_URL=http://127.0.0.1:8995/releases
# shellcheck disable=SC2086
if sudo -E sh install.sh --yes --auto-update >auto.out 2>&1; then cat auto.out; fail "the public check passed on a rerun"; fi
ends_at_public_check auto.out "a rerun turning automatic updates on"
unit=/etc/systemd/system/arr-update.path
sudo test -f "$unit" || fail "no arr-update.path"
sudo grep -qx "PathExists=$state/update-requested" "$unit" || { sudo cat "$unit"; fail "the path unit watches the wrong file"; }
sudo grep -qx 'Unit=arr-update.service' "$unit" || fail "the path unit starts the wrong service"
systemctl is-active arr-update.path >/dev/null || fail "arr-update.path isn't active"
systemctl is-enabled arr-update.path >/dev/null || fail "arr-update.path isn't enabled"
if command -v systemd-analyze >/dev/null 2>&1; then
	sudo systemd-analyze verify "$unit" /etc/systemd/system/arr-update.service || fail "systemd finds a problem with the path unit"
fi
sudo grep -q 'arr-update-trigger' /usr/local/bin/arr-update || fail "the updater doesn't know the request file"
# What Arr does: it makes the file (as the user that runs it, not root) and nothing else.
rm -f "$state/update-status.json"
touch "$state/update-requested"
i=0
until grep -q '"state":"up_to_date"' "$state/update-status.json" 2>/dev/null; do
	i=$((i + 1))
	[ "$i" -lt 60 ] || { sudo journalctl -u arr-update --no-pager | tail -n 20; fail "the updater didn't run after the request"; }
	sleep 1
done
[ ! -e "$state/update-requested" ] || fail "the updater left the request file"
python3 - "$state/update-status.json" <<'PY' || fail "update-status.json isn't what Arr reads"
import json, sys
s = json.load(open(sys.argv[1]))
assert s["state"] == "up_to_date" and s["to"] == "0.0.1" and s["from"].count(".") == 2 and s["at"].endswith("Z"), s
PY
# One request, one run: the file is gone, so systemd does not start it again.
sleep 3
[ "$(sudo journalctl -u arr-update --no-pager | grep -c 'arr-update: up to date')" = 1 ] || fail "one request ran the updater more than once"
still_running "an update request"
kill $relsrv 2>/dev/null || true
sudo systemctl unset-environment ARR_RELEASES_URL

echo "== uninstall Arr and its data"
sudo sh uninstall.sh --yes --arr --delete-data
for u in arr arr-tunnel arr-update.timer arr-update.path; do
	[ ! -e "/etc/systemd/system/$u.service" ] && [ ! -e "/etc/systemd/system/$u" ] || fail "$u is still installed"
done
[ ! -e /usr/local/bin/arr ] || fail "the arr program is still there"
[ ! -e "$state" ] || fail "Arr's data is still there"
pgrep -f "$state/ngrok.yml" >/dev/null && fail "ngrok is still running"
echo "linux installer: all checks passed"
