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

echo "== a rerun keeps remote access"
sh install.sh "$@" >rerun.out 2>&1 || true
grep -q 'already has remote access; keeping it' rerun.out || { cat rerun.out; fail "rerun didn't keep remote access"; }
[ "$(cat "$state/config.toml")" = "$before" ] || fail "a rerun changed the config"
[ -x "$HOME/.local/bin/arr.previous" ] || fail "the rerun didn't keep the previous binary"
still_running "a rerun"

echo "== a rerun with a first-setup-only option is refused, and changes nothing"
if sh install.sh --yes --no-auto-update --movies /elsewhere >refused.out 2>&1; then
	cat refused.out; fail "--movies on an existing install was accepted"
fi
cat refused.out
grep -q -- '--movies only applies to the first setup' refused.out || fail "the refusal doesn't say why"
[ "$(cat "$state/config.toml")" = "$before" ] || fail "a refused rerun changed the config"
still_running "a refused rerun"

echo "== a rerun applies a setting it's given"
sh install.sh --yes --no-auto-update --no-error-reports >setting.out 2>&1 || true
grep -A1 '^\[reporting\]$' "$state/config.toml" | grep -q '^enabled = false$' || { cat setting.out; fail "--no-error-reports wasn't applied"; }
still_running "a rerun with --no-error-reports"

echo "== running as root on a Mac is refused"
if sudo sh install.sh --yes >root.out 2>&1; then cat root.out; fail "install.sh ran as root on a Mac"; fi
grep -q 'without sudo' root.out || { cat root.out; fail "the refusal doesn't say to run it without sudo"; }

echo "== uninstall Arr and Plex, keeping media"
sh uninstall.sh --yes --all --delete-data
[ ! -e "$app" ] || fail "the Plex app is still there"
[ ! -e "$HOME/Library/LaunchAgents/io.github.newtonharry.plex.plist" ] || fail "Plex's login agent is still there"
for p in arr arr-tunnel; do
	[ ! -e "$HOME/Library/LaunchAgents/io.github.newtonharry.$p.plist" ] || fail "$p login agent is still there"
done
[ ! -e "$state" ] || fail "Arr's data is still there"
[ ! -e "$HOME/.local/bin/arr" ] || fail "the arr program is still there"
[ -f "$HOME/Media/Movies/keep.mkv" ] || fail "media was deleted"
echo "macos installer: all checks passed"
