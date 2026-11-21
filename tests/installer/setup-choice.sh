#!/bin/sh
# CI (Linux): how install.sh chooses and runs the first setup, against a stand-in arr that
# records how it was run (no real binary, Plex or service; systemctl, loginctl, sudo and docker
# are stand-ins too, and HOME is a made-up one):
#
#   - ARR_AUTO_UPDATE, ARR_ERROR_REPORTS and ARR_BROWSER take only 1 or 0;
#   - at a terminal with no setup options, "web browser or terminal?" is asked, and Enter means
#     the setup page; a TMDB key doesn't change that, and is added after the page;
#   - it is asked over SSH too, and the page then listens on the SSH address (only a private or
#     loopback one is checked for a busy port, as the page uses 127.0.0.1 otherwise);
#   - it isn't asked with --yes, with --terminal, with setup options (which say so) or with
#     --browser;
#   - "Open Arr" is promised only for a page on a loopback address;
#   - an automatic updates option removes the page's answer kept for the updater step (after
#     the page, and at the start of a re-run), without following a link planted there; an
#     answer left from an earlier attempt is removed before a first setup;
#   - after the page Arr starts before the remaining questions, the state folder is private
#     even if it existed with mode 755, the page doesn't inherit a private umask, unit files
#     are mode 644, and the page's automatic updates answer is used and then removed;
#   - --no-service says how to start Arr right after the page; a busy port stops before the
#     page, naming --port (on 127.0.0.1 and on the page's IPv6 address); a page that fails
#     doesn't claim that nothing was saved.
#
# No case can reach a real Plex: curl refuses Plex's port 32400. --yes and --terminal try it
# (refused); every other case that gets to the terminal setup gives a closed Plex address.
#
# Usage: sh tests/installer/setup-choice.sh      (from the repository root)
set -eu
fail() { echo "FAIL: $*" >&2; exit 1; }
repo=$(cd "$(dirname "$0")/../.." && pwd)
case "$(uname -m)" in x86_64 | amd64) target=x86_64-unknown-linux-gnu ;; aarch64 | arm64) target=aarch64-unknown-linux-gnu ;; *) fail "no target for $(uname -m)" ;; esac
[ -d /run/systemd/system ] || fail "needs a systemd machine (install.sh looks for /run/systemd/system)"
freeport() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port=$(freeport)
relport=$(freeport)

w=$(mktemp -d)
export FAKE_DIR="$w/fake" FAKE_PORT="$port"
stop_health() { [ ! -f "$FAKE_DIR/health.pid" ] || { kill "$(cat "$FAKE_DIR/health.pid")" 2>/dev/null || true; rm -f "$FAKE_DIR/health.pid"; }; }
trap 'stop_health; [ -z "${busy:-}" ] || kill "$busy" 2>/dev/null || true; [ -z "${relpid:-}" ] || kill "$relpid" 2>/dev/null || true; rm -rf "$w"' EXIT
mkdir -p "$FAKE_DIR/bin" "$FAKE_DIR/www/api/v1" "$w/dist/arr-$target"
echo '{"status":"ok"}' >"$FAKE_DIR/www/api/v1/health"
unset SSH_CONNECTION SSH_CLIENT SSH_TTY XDG_DATA_HOME XDG_CONFIG_HOME ARR_PORT ARR_YES
# A display, so the page would listen on this computer only (127.0.0.1) and open itself.
export DISPLAY=:0
umask 022

# The stand-in arr, shipped as the release. The setup page ("wizard") checks the state folder
# as the real one does and writes a config without Plex, so nothing is imported afterwards.
cat >"$w/dist/arr-$target/arr" <<'EOF'
#!/bin/sh
echo "arr $*" >>"$FAKE_DIR/arr.log"
case "$1" in
--version) echo "arr 0.20.0" ;;
wizard)
	umask >"$FAKE_DIR/wizard.umask"
	while [ $# -gt 0 ]; do
		case "$1" in --state-dir) sd=$2; shift ;; --port) p=$2; shift ;; esac
		shift
	done
	[ "$(stat -c %a "$sd")" = 700 ] || { echo "Error: $sd can be read by others" >&2; exit 1; }
	[ "${FAKE_WIZARD_FAIL:-0}" = 0 ] || { echo "Error: setup stopped" >&2; exit 1; }
	mkdir -p "$sd/secrets"
	printf 'tok' >"$sd/secrets/api_token"
	printf '[server]\nlisten = "127.0.0.1:%s"\napi_token_file = "secrets/api_token"\n\n[execution]\nenabled = true\n' "$p" >"$sd/config.toml"
	chmod 600 "$sd/config.toml"
	printf 'auto_update=%s\n' "${FAKE_AUTO_UPDATE:-0}" >"$sd/setup-choices"
	;;
set)
	case "$*" in *tmdb-key*) cat >"$FAKE_DIR/tmdb.stdin" ;; esac
	echo "Saved."
	;;
app-link) exit 1 ;;
esac
exit 0
EOF
chmod +x "$w/dist/arr-$target/arr"
tar -C "$w/dist" -czf "$w/dist/arr-$target.tar.gz" "arr-$target"
(cd "$w/dist" && sha256sum "arr-$target.tar.gz" >"arr-$target.tar.gz.sha256")
python3 -m http.server "$relport" --bind 127.0.0.1 --directory "$w/dist" >/dev/null 2>&1 &
relpid=$!
export ARR_DOWNLOAD_BASE="http://127.0.0.1:$relport"
i=0
until curl -fsS -o /dev/null "$ARR_DOWNLOAD_BASE/arr-$target.tar.gz.sha256" 2>/dev/null; do
	i=$((i + 1)); [ "$i" -lt 50 ] || fail "the stand-in release server didn't start"; sleep 0.2
done

# systemctl: "restart arr" starts a stand-in that answers Arr's health check.
cat >"$FAKE_DIR/bin/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >>"$FAKE_DIR/calls.log"
case "$*" in
*"restart arr")
	if [ ! -f "$FAKE_DIR/health.pid" ]; then
		python3 -m http.server "$FAKE_PORT" --bind 127.0.0.1 --directory "$FAKE_DIR/www" >/dev/null 2>&1 </dev/null &
		echo $! >"$FAKE_DIR/health.pid"
	fi ;;
esac
EOF
cat >"$FAKE_DIR/bin/loginctl" <<'EOF'
#!/bin/sh
case "$*" in *show-user*) echo "Linger=no" ;; esac
EOF
cat >"$FAKE_DIR/bin/sudo" <<'EOF'
#!/bin/sh
echo "sudo $*" >>"$FAKE_DIR/calls.log"
echo "sudo: a password is required" >&2
exit 1
EOF
printf '#!/bin/sh\nexit 0\n' >"$FAKE_DIR/bin/docker"
printf '#!/bin/sh\necho "ffprobe version stand-in"\n' >"$FAKE_DIR/bin/ffprobe"
printf '#!/bin/sh\necho "xdg-open $*" >>"$FAKE_DIR/calls.log"\n' >"$FAKE_DIR/bin/xdg-open"
# This test must never reach a real Plex (one may run on this computer): curl refuses Plex's
# port (as "couldn't connect"), so Plex's token file is never read either. --yes and --terminal
# try that port (with no setup option, so it's the flag that chooses the terminal setup); every
# other case that gets to the terminal setup gives a Plex address on a closed port.
REAL_CURL=$(command -v curl) || fail "no curl"
export REAL_CURL
cat >"$FAKE_DIR/bin/curl" <<'EOF'
#!/bin/sh
case "$*" in *:32400*) echo "curl $*" >>"$FAKE_DIR/plex.log"; exit 7 ;; esac
exec "$REAL_CURL" "$@"
EOF
chmod +x "$FAKE_DIR/bin"/*
export PATH="$FAKE_DIR/bin:$PATH"
noplex="http://127.0.0.1:$(freeport)"

question='Set up Arr in your web browser, or here in the terminal?'
n=0
# A new made-up HOME (no Arr installed yet) for each run.
fresh() {
	n=$((n + 1))
	stop_health
	export HOME="$w/home$n"
	mkdir -p "$HOME"
	state="$HOME/.local/share/arr"
	: >"$FAKE_DIR/arr.log"
	: >"$FAKE_DIR/calls.log"
	rm -f "$FAKE_DIR/wizard.umask" "$FAKE_DIR/tmdb.stdin"
}
out="$w/out"
at_terminal() { python3 "$repo/tests/installer/fakepty.py" sh "$repo/install.sh" "$@" >"$out" 2>&1; }
unattended() { sh "$repo/install.sh" "$@" >"$out" 2>&1 </dev/null; }
asked() { grep -qF "$question" "$out"; }
wizard_ran() { grep -q '^arr wizard ' "$FAKE_DIR/arr.log"; }
# The case just run tried Plex's own address, which curl refused: only its /identity, and
# nothing else of Plex's, then the log is cleared for the check at the end.
plex_refused() {
	[ -s "$FAKE_DIR/plex.log" ] || { cat "$out"; fail "$1 didn't try Plex's address"; }
	! grep -qv ':32400/identity' "$FAKE_DIR/plex.log" || { cat "$FAKE_DIR/plex.log"; fail "$1 asked Plex for more than /identity"; }
	: >"$FAKE_DIR/plex.log"
}
# Line number of the first line of $out containing $1 (0 if none).
line_of() { grep -nF -- "$1" "$out" | head -n1 | cut -d: -f1 | grep . || echo 0; }

echo "== settings from the environment take only 1 or 0"
for v in ARR_AUTO_UPDATE=true ARR_ERROR_REPORTS=yes ARR_BROWSER=2; do
	fresh
	rc=0; env "$v" sh "$repo/install.sh" --yes >"$out" 2>&1 </dev/null || rc=$?
	[ "$rc" = 2 ] || { cat "$out"; fail "$v: exit $rc, wanted 2"; }
	grep -q "^${v%%=*} must be 1 or 0, not '${v#*=}'$" "$out" || { cat "$out"; fail "$v: no explanation"; }
done
rc=0; sh "$repo/install.sh" --browser --terminal >"$out" 2>&1 </dev/null || rc=$?
[ "$rc" = 2 ] && grep -q "can't be used together" "$out" || { cat "$out"; fail "--browser with --terminal wasn't refused"; }
fresh
if unattended --browser --movies /nowhere --port "$port"; then cat "$out"; fail "--browser with --movies was accepted"; fi
grep -q -- "--browser can't be used with --movies" "$out" || { cat "$out"; fail "--browser with --movies: no explanation"; }

echo "== at a terminal: asked, Enter is the setup page, Arr starts before the other questions"
fresh
mkdir -p "$state"
chmod 755 "$state"
key=ci-tmdb-key-0123456789
FAKE_AUTO_UPDATE=1 at_terminal --port "$port" --no-at-boot --tmdb-key "$key" || { cat "$out"; fail "the setup-page install failed"; }
asked || { cat "$out"; fail "the question wasn't asked"; }
wizard_ran || { cat "$out"; fail "Enter didn't choose the setup page"; }
grep -q "^arr wizard --state-dir $state --bind 127.0.0.1 --port $port " "$FAKE_DIR/arr.log" || { cat "$FAKE_DIR/arr.log"; fail "the page wasn't started as expected"; }
[ "$(stat -c %a "$state")" = 700 ] || fail "the state folder isn't private"
[ "$(cat "$FAKE_DIR/wizard.umask")" = 0022 ] || fail "the page ran with umask $(cat "$FAKE_DIR/wizard.umask"), not the user's 0022"
running=$(line_of "the setup page's \"Open Arr\" button works now")
remote=$(line_of "Set that up now?")
[ "$running" -gt 0 ] && [ "$remote" -gt "$running" ] || { cat "$out"; fail "Arr didn't start before the remote access question"; }
grep -q '^systemctl --user restart arr$' "$FAKE_DIR/calls.log" || fail "Arr wasn't started"
[ "$(grep -c '^systemctl --user restart arr$' "$FAKE_DIR/calls.log")" = 1 ] || fail "Arr was restarted with nothing changed"
units="$HOME/.config/systemd/user"
for f in arr.service arr-update.service arr-update.timer arr-update.path; do
	[ -f "$units/$f" ] || { cat "$out"; fail "no $f"; }
	[ "$(stat -c %a "$units/$f")" = 644 ] || fail "$f is mode $(stat -c %a "$units/$f"), not 644"
done
grep -q 'Install new versions of Arr automatically' "$out" && fail "the page's automatic updates answer was asked again"
[ ! -e "$state/setup-choices" ] || fail "setup-choices was left after the updater step"
grep -q "^arr set --config $state/config.toml tmdb-key$" "$FAKE_DIR/arr.log" || { cat "$FAKE_DIR/arr.log"; fail "the TMDB key wasn't added after the page"; }
[ "$(cat "$FAKE_DIR/tmdb.stdin")" = "$key" ] || fail "the TMDB key didn't reach arr on its standard input"
grep -q 'sudo' "$FAKE_DIR/calls.log" && fail "sudo was called"

echo "== over SSH: asked too; the page listens on the SSH address, where its button works"
fresh
(export SSH_CONNECTION="::1 50000 ::1 22" && FAKE_AUTO_UPDATE=0 at_terminal --port "$port" --no-at-boot) || { cat "$out"; fail "the install over SSH failed"; }
asked || { cat "$out"; fail "the question wasn't asked over SSH"; }
grep -q "^arr wizard --state-dir $state --bind ::1 --port $port " "$FAKE_DIR/arr.log" || { cat "$FAKE_DIR/arr.log"; fail "the page didn't listen on the SSH address"; }
grep -qF "Arr is running: the setup page's \"Open Arr\" button works now." "$out" || { cat "$out"; fail "the button isn't promised on a loopback page"; }

echo "== over SSH to a public address: the page uses 127.0.0.1 instead, so only that is checked"
fresh
(export SSH_CONNECTION="2001:db8::2 50000 2001:db8::1 22" && FAKE_AUTO_UPDATE=0 at_terminal --port "$port" --no-at-boot) || { cat "$out"; fail "the install over SSH to a public address failed"; }
grep -q "^arr wizard --state-dir $state --bind 2001:db8::1 --port $port " "$FAKE_DIR/arr.log" || { cat "$FAKE_DIR/arr.log"; fail "the page wasn't given the SSH address"; }
grep -qF "Arr is running: the setup page's \"Open Arr\" button works now." "$out" || { cat "$out"; fail "the button isn't promised though the page falls back to 127.0.0.1"; }

echo "== no screen: the page listens on the network address, and its button isn't promised"
fresh
(unset DISPLAY && FAKE_AUTO_UPDATE=0 at_terminal --port "$port" --no-at-boot) || { cat "$out"; fail "the install without a screen failed"; }
bind=$(sed -n 's/^arr wizard .* --bind \([^ ]*\) .*/\1/p' "$FAKE_DIR/arr.log")
if [ "$bind" = 127.0.0.1 ]; then
	echo "(skipped: this computer has no network address)"
else
	tr -d '\r' <"$out" | grep -qx 'Arr is running.' || { cat "$out"; fail "no plain 'Arr is running.' for a page on $bind"; }
	! grep -q '"Open Arr" button works now' "$out" || { cat "$out"; fail "the button was promised on a page that hides it"; }
fi

echo "== --yes: not asked, terminal setup; a page answer left from an earlier attempt goes first"
fresh
mkdir -p "$state"
echo auto_update=1 >"$state/setup-choices"
unattended --yes --port "$port" || true
! asked || { cat "$out"; fail "--yes asked"; }
! wizard_ran || fail "--yes ran the setup page"
grep -q '^==> Finding Plex' "$out" || { cat "$out"; fail "--yes didn't run the terminal setup"; }
plex_refused --yes
[ ! -e "$state/setup-choices" ] || fail "an earlier attempt's setup-choices was left for this setup"

echo "== --terminal: not asked, terminal setup"
fresh
at_terminal --terminal --port "$port" || true
! asked || { cat "$out"; fail "--terminal asked"; }
! wizard_ran || fail "--terminal ran the setup page"
grep -q '^==> Finding Plex' "$out" || { cat "$out"; fail "--terminal didn't run the terminal setup"; }
plex_refused --terminal

echo "== setup options: not asked, terminal setup, and it says why"
fresh
at_terminal --movies "$w/nowhere" --plex-url "$noplex" --port "$port" || true
! asked || { cat "$out"; fail "setup options asked"; }
! wizard_ran || fail "setup options ran the setup page"
grep -qF 'Setting Arr up here in the terminal, because setup options were given (--plex-url, --movies).' "$out" || { cat "$out"; fail "no reason given for the terminal setup"; }

echo "== --browser --no-service: not asked, and says how to start Arr right after the page"
fresh
FAKE_AUTO_UPDATE=0 unattended --browser --yes --no-service --port "$port" || { cat "$out"; fail "--browser --no-service failed"; }
! asked || fail "--browser asked"
wizard_ran || fail "--browser didn't run the setup page"
grep -q 'No background service will be installed, so Arr doesn' "$out" || { cat "$out"; fail "no start command after the page"; }
grep -qF "  \"$HOME/.local/bin/arr\" serve --config \"$state/config.toml\"" "$out" || { cat "$out"; fail "the start command isn't shown"; }
# With no service the run ends before the automatic updates step, so the page's answer waits for it.
[ -e "$state/setup-choices" ] || fail "setup-choices was removed before the updater step used it"

echo "== a re-run with --auto-update: the page's older answer goes, and a link there isn't followed"
# --no-service ends this run too before the updater step, like one that was interrupted there.
unattended --yes --auto-update --no-service --port "$port" || { cat "$out"; fail "the re-run failed"; }
[ ! -e "$state/setup-choices" ] || fail "setup-choices still holds the page's older answer: $(cat "$state/setup-choices")"
echo keep >"$w/target"
ln -s "$w/target" "$state/setup-choices"
unattended --yes --no-auto-update --no-service --port "$port" || { cat "$out"; fail "the re-run with a link failed"; }
[ ! -e "$state/setup-choices" ] && [ ! -L "$state/setup-choices" ] || fail "the link in place of setup-choices was left"
[ "$(cat "$w/target")" = keep ] || fail "the file a link pointed to was written"

echo "== --no-auto-update with the setup page: the page's answer is removed straight away"
fresh
FAKE_AUTO_UPDATE=1 unattended --browser --yes --no-service --no-auto-update --port "$port" || { cat "$out"; fail "--browser --no-auto-update failed"; }
wizard_ran || fail "the setup page didn't run"
[ ! -e "$state/setup-choices" ] || fail "the page's answer was kept though --no-auto-update was given"

echo "== a page that fails: no claim that nothing was saved"
fresh
if FAKE_WIZARD_FAIL=1 unattended --browser --yes --port "$port"; then cat "$out"; fail "a failed page passed"; fi
grep -q "setup wasn't finished, so Arr isn't set up yet" "$out" || { cat "$out"; fail "the failure message changed"; }
grep -q 'nothing was saved' "$out" && fail "the failure still says nothing was saved"

echo "== a busy port: stops before the page, naming --port"
fresh
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$w" >/dev/null 2>&1 &
busy=$!
i=0
until curl -fsS -o /dev/null "http://127.0.0.1:$port/" 2>/dev/null; do i=$((i + 1)); [ "$i" -lt 30 ] || fail "the busy-port stand-in didn't start"; sleep 0.2; done
if at_terminal --port "$port"; then cat "$out"; fail "a busy port passed"; fi
grep -q "port $port on this computer is already in use.*--port" "$out" || { cat "$out"; fail "the busy port isn't explained"; }
! wizard_ran || fail "the page ran on a busy port"
! asked || fail "asked how to set up with a busy port"
kill "$busy"; busy=""

echo "== a busy port on the page's IPv6 address: stops before the page, naming --port"
fresh
python3 -m http.server "$port" --bind ::1 --directory "$w" >/dev/null 2>&1 &
busy=$!
i=0
until curl -g -fsS -o /dev/null "http://[::1]:$port/" 2>/dev/null; do i=$((i + 1)); [ "$i" -lt 30 ] || fail "the IPv6 busy-port stand-in didn't start"; sleep 0.2; done
if (export SSH_CONNECTION="::1 50000 ::1 22" && at_terminal --port "$port"); then cat "$out"; fail "a busy IPv6 port passed"; fi
grep -q "port $port on this computer is already in use.*--port" "$out" || { cat "$out"; fail "the busy IPv6 port isn't explained"; }
! wizard_ran || fail "the page ran on a busy IPv6 port"
kill "$busy"; busy=""

[ ! -s "$FAKE_DIR/plex.log" ] || { cat "$FAKE_DIR/plex.log"; fail "a case tried Plex's own address"; }
echo "setup choice: all checks passed"
