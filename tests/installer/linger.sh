#!/bin/sh
# CI (Linux): a per-user install.sh and "lingering", which makes a user systemd service start at
# boot and survive logout. Everything that would touch the machine is a stand-in (systemctl,
# loginctl, sudo, with a made-up HOME), so this never starts or stops a real service:
#
#   - nobody to ask (--yes): lingering isn't turned on without a password, and the install says
#     what to run; sudo is never called;
#   - --at-boot with nobody at a terminal: sudo is tried (never waiting for a password), and
#     the install carries on if it refuses;
#   - --no-at-boot: sudo is not called and nothing is said to be wrong;
#   - at a terminal: one sudo prompt (loginctl enable-linger) when polkit says no;
#   - already lingering: nothing to do.
#
# Usage: sh tests/installer/linger.sh path/to/arr      (from the repository root)
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

w=$(mktemp -d)
trap 'pkill -f "serve --config $w/" 2>/dev/null || true; [ -z "${relpid:-}" ] || kill "$relpid" 2>/dev/null || true; rm -rf "$w"' EXIT
export HOME="$w/home"
unset XDG_DATA_HOME XDG_CONFIG_HOME
mkdir -p "$HOME" "$w/fake/bin" "$w/dist"
state="$HOME/.local/share/arr"
calls="$w/fake/calls.log"
export FAKE_DIR="$w/fake"

cat >"$w/fake/bin/systemctl" <<'EOF'
#!/bin/sh
echo "systemctl $*" >>"$FAKE_DIR/calls.log"
EOF
# loginctl: enable-linger works only as root (through sudo), as when polkit refuses a plain user.
cat >"$w/fake/bin/loginctl" <<'EOF'
#!/bin/sh
echo "root=${FAKE_ROOT:-0} loginctl $*" >>"$FAKE_DIR/calls.log"
case "$*" in
*enable-linger*)
	[ "${FAKE_ROOT:-0}" = 1 ] || { echo "Failed to enable linger: Interactive authentication required." >&2; exit 1; }
	echo yes >"$FAKE_DIR/linger" ;;
*show-user*) echo "Linger=$(cat "$FAKE_DIR/linger" 2>/dev/null || echo no)" ;;
esac
EOF
# sudo: -n refuses when a password would be needed (FAKE_SUDO_PASSWORD=1) or always (FAKE_SUDO_DENY=1).
cat >"$w/fake/bin/sudo" <<'EOF'
#!/bin/sh
echo "sudo $*" >>"$FAKE_DIR/calls.log"
if [ "${FAKE_SUDO_DENY:-0}" = 1 ]; then echo "sudo: a password is required" >&2; exit 1; fi
if [ "$1" = -n ] && [ "${FAKE_SUDO_PASSWORD:-0}" = 1 ]; then echo "sudo: a password is required" >&2; exit 1; fi
[ "$1" != -n ] || shift
FAKE_ROOT=1 exec "$@"
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

run() { : >"$calls"; sh "$repo/install.sh" --no-auto-update "$@" >"$w/out" 2>&1 </dev/null || { cat "$w/out"; fail "install.sh $* failed"; }; }
sudo_calls() { grep -c '^sudo' "$calls" || true; }
no_warning() { ! grep -q 'Arr stops when you log out' "$w/out" || { cat "$w/out"; fail "$1"; }; }

echo "== nobody to ask: no sudo, and it says what to run"
rm -f "$FAKE_DIR/linger"
run --yes
grep -q 'Warning: Arr stops when you log out' "$w/out" || { cat "$w/out"; fail "no warning"; }
grep -q 'sudo loginctl enable-linger' "$w/out" || fail "the warning doesn't say what to run"
[ "$(sudo_calls)" = 0 ] || { cat "$calls"; fail "an unattended install called sudo"; }

echo "== --at-boot with no terminal: sudo -n is tried, and the install carries on if it is refused"
rm -f "$FAKE_DIR/linger"
FAKE_SUDO_PASSWORD=1 run --yes --at-boot </dev/null
grep -q 'sudo -n loginctl enable-linger\|sudo loginctl enable-linger' "$calls" || { cat "$calls"; fail "sudo wasn't tried"; }
grep -q 'Warning: Arr stops when you log out' "$w/out" || { cat "$w/out"; fail "no warning after the refusal"; }
rm -f "$FAKE_DIR/linger"
run --yes --at-boot
grep -q 'root=1 loginctl enable-linger' "$calls" || { cat "$calls"; fail "lingering wasn't turned on through sudo"; }
no_warning "a warning after lingering was turned on"
[ "$(cat "$FAKE_DIR/linger")" = yes ] || fail "lingering isn't on"

echo "== already lingering: nothing to ask"
run --yes --at-boot
[ "$(sudo_calls)" = 0 ] || { cat "$calls"; fail "sudo was called with lingering already on"; }
no_warning "a warning with lingering on"

echo "== --no-at-boot: no sudo, no warning"
rm -f "$FAKE_DIR/linger"
run --yes --no-at-boot
[ "$(sudo_calls)" = 0 ] || fail "--no-at-boot called sudo"
no_warning "--no-at-boot warned"
grep -q 'Not turning on lingering' "$w/out" || fail "--no-at-boot didn't say what it does"

echo "== at a terminal: one sudo, no flags needed"
rm -f "$FAKE_DIR/linger"
: >"$calls"
python3 "$repo/tests/installer/fakepty.py" sh "$repo/install.sh" --no-auto-update >"$w/out" 2>&1 || { cat "$w/out"; fail "the terminal install failed"; }
grep -q 'needs your administrator password once' "$w/out" || { cat "$w/out"; fail "no explanation before the sudo prompt"; }
[ "$(grep -c '^sudo loginctl enable-linger' "$calls")" = 1 ] || { cat "$calls"; fail "expected exactly one sudo loginctl enable-linger"; }
[ "$(cat "$FAKE_DIR/linger")" = yes ] || fail "lingering isn't on after the prompt"

echo "linger: all checks passed"
