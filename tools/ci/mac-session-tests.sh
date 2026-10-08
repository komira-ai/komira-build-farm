#!/usr/bin/env bash
# kbf-mac-session for real, as root, on a macOS runner (docs/design/fleet-updates-security.md
# S4.2, S4.3 and S10). The unit tests run the verbs' rules on every platform against a fake
# host; this runs the macOS host itself: OpenDirectory users, launchd, libproc, the sweep,
# and the caller check by code signature.
#
# The helper is signed ad hoc with the hardened runtime (tools/ci/sign-darwin.sh) and
# started under sudo with the cdhash of examples/mac_session_client.rs as the daemon
# requirement, so that binary stands in for kbf-daemon. A copy signed under another
# identifier (another cdhash) is the impostor.
#
# GitHub's hosted macOS runners allow passwordless sudo. Where it is not allowed this
# script says so and stops after building and signing.
set -euo pipefail

export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-14.0}"
cargo build --locked -p kbf-mac-session --bin kbf-mac-session --example mac_session_client
helper=target/debug/kbf-mac-session
# Everything the lease users and `nobody` must reach lives where every user may search:
# the runner's home is not.
tmp=$(mktemp -d /private/tmp/kbf-ci.XXXXXX)
chmod 0755 "$tmp"
client="$tmp/client"
impostor="$tmp/impostor"
cp target/debug/examples/mac_session_client "$client"
cp target/debug/examples/mac_session_client "$impostor"
bash tools/ci/sign-darwin.sh "$helper"
codesign --force --sign - --options runtime "$client"
codesign --force --sign - --options runtime --identifier kbf.ci.impostor "$impostor"
chmod 0755 "$client" "$impostor"
cdhash() { codesign --display --verbose=4 "$1" 2>&1 | sed -n 's/^CDHash=//p'; }
genuine=$(cdhash "$client")
other=$(cdhash "$impostor")
echo "client cdhash $genuine, impostor cdhash $other"
[ -n "$genuine" ] && [ "$genuine" != "$other" ]

if ! sudo -n true 2>/dev/null; then
  echo "::warning::no passwordless sudo on this runner: kbf-mac-session was built and signed but not run as root"
  exit 0
fi

users=/Users
shared="$users/Shared"
fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo; echo "--- $*"; }
# expect_refused <pattern> <command...>: the command must fail, saying <pattern>.
expect_refused() {
  local pattern=$1 out
  shift
  if out=$("$@" 2>&1); then fail "expected a refusal from: $* (got: $out)"; fi
  grep -q -- "$pattern" <<<"$out" || fail "refused, but not for '$pattern': $out"
  echo "refused as expected: $out"
}
owner_mode() { sudo -n stat -f '%u %Lp' "$1"; }

# Free uids for the leases, and a group the runner is in that is neither staff nor the
# lease users' group (the helper refuses those): the runner is an administrator.
first=650 last=659
in_use=$(dscl . -list /Users UniqueID | awk -v a=$first -v b=$last '$2 >= a && $2 <= b')
[ -z "$in_use" ] || fail "uids $first-$last are in use: $in_use"
id -Gn | tr ' ' '\n' | grep -qx admin || fail "the runner is not in the admin group"
serial=$(ioreg -rd1 -c IOPlatformExpertDevice | sed -n 's/.*"IOPlatformSerialNumber" = "\(.*\)"/\1/p')
echo "serial: $serial"

run_dir=/private/var/run/kbf-ci-mac-session
state=/private/var/db/kbf-ci-mac-session
socket="$run_dir/socket"
sudo -n rm -rf "$run_dir" "$state"
"$client" pubkey 07 > "$tmp/grant-keys"
sudo -n "$helper" --socket "$socket" --socket-group admin --state-dir "$state" \
  --uid-range "$first-$last" --daemon-requirement "cdhash H\"$genuine\"" \
  --grant-keys "$tmp/grant-keys" > "$tmp/helper.log" 2>&1 &
cleanup() {
  sudo -n pkill -f "$helper --socket" || true
  echo "--- helper log"; cat "$tmp/helper.log" || true
}
trap cleanup EXIT
for _ in $(seq 100); do [ -S "$socket" ] && break; sleep 0.1; done
[ -S "$socket" ] || fail "the helper did not bind $socket"

step "S4.3: the socket admits only its group"
[ "$(stat -f '%Lp %Sg' "$run_dir")" = "750 admin" ] || fail "socket directory: $(stat -f '%Lp %Sg' "$run_dir")"
[ "$(stat -f '%Lp %Sg' "$socket")" = "660 admin" ] || fail "socket: $(stat -f '%Lp %Sg' "$socket")"
expect_refused "ermission denied" sudo -n -u nobody "$client" kill "$socket" 1.1

step "user-create: a fresh non-admin user with a private home"
uid=$("$client" create "$socket" 1.1)
[ "$uid" = "$first" ] || fail "uid $uid, want $first"
[ "$(id -u kbf-lease-1-1)" = "$first" ] || fail "id: $(id kbf-lease-1-1)"
[ "$(id -gn kbf-lease-1-1)" = staff ] || fail "group: $(id -gn kbf-lease-1-1)"
if dseditgroup -o checkmember -m kbf-lease-1-1 admin; then fail "kbf-lease-1-1 is an administrator"; fi
[ "$(owner_mode "$users"/kbf-lease-1-1)" = "$first 700" ] || fail "home: $(owner_mode "$users"/kbf-lease-1-1)"
dscl . -read "$users"/kbf-lease-1-1 IsHidden | grep -q 1 || fail "the user is not hidden"
sudo -n sysadminctl -secureTokenStatus kbf-lease-1-1 2>&1 | tee /dev/stderr | grep -q DISABLED \
  || fail "the lease user has a secure token"

step "run: as the lease user, in the lease directory"
lease_dir=$(mktemp -d "$tmp/lease.XXXXXX")
# The daemon opens the lease directory and the lease user works in it (on a node an
# inherited ACL entry gives the daemon access; here the directory is open to all).
chmod 0777 "$lease_dir"
out=$("$client" run "$socket" 1.1 "$lease_dir" /bin/sh -c 'id -u; pwd -P; echo "$HOME"')
want=$(printf '%s\n%s\n%s\nexit 0' "$first" "$(cd "$lease_dir" && pwd -P)" "$users"/kbf-lease-1-1)
[ "$out" = "$want" ] || fail "run printed: $out"
[ "$("$client" run "$socket" 1.1 "$lease_dir" /bin/sh -c 'exit 7')" = "exit 7" ] || fail "exit code"

step "S4.3 control 1: an action cannot reach the helper"
out=$("$client" run "$socket" 1.1 "$lease_dir" "$client" kill "$socket" 1.1 2>&1)
grep -q "exit 4" <<<"$out" || fail "the lease user reached the socket: $out"
grep -q "ermission denied" <<<"$out" || fail "not refused by the socket's permissions: $out"
echo "the lease user was kept out: $out"

step "S10: a caller that is not the daemon is refused; so is one that connects, then executes it"
expect_refused "caller refused" "$impostor" kill "$socket" 1.1
# The impostor connects, waits for the helper's answer, then executes the genuine client,
# which carries out the whole exchange on that connection: refused, because the helper
# checked the impostor at accept.
expect_refused "caller refused" "$impostor" connect-then-exec "$socket" "$client" kill "$socket" 1.1
# The impostor writes its request at once, then executes the genuine client, which only
# reads: refused at accept, or (had the exec won the race to accept) for the nonce.
expect_refused "caller refused" "$impostor" write-then-exec "$socket" 1.1 "$client" replies
# What the audit token says across an exec, measured on this macOS (no helper involved).
"$client" exec-probe
# The control: the genuine client over the same path is accepted (no process to kill).
"$client" kill "$socket" 1.1
echo "the helper's view of those callers (pid and pid version at accept and at the request):"
grep -E "caller (pid|refused)|refused a caller" "$tmp/helper.log" | tail -n 8 || true

step "plant state the sweep must remove, and a process that outlives its action"
sudo -n mkdir -p /private/var/kbf-ci-sentinel
sudo -n touch /private/var/kbf-ci-sentinel/keep
override="/private/var/db/com.apple.xpc.launchd/disabled.$first.plist"
if sudo -n launchctl disable "user/$first/kbf.ci.planted" && sudo -n test -e "$override"; then
  echo "planted $override"
else
  echo "::warning::launchctl wrote no per-uid override here; that sweep item is not exercised"
  override=""
fi
"$client" run "$socket" 1.1 "$lease_dir" /bin/sh -c '
  set -e
  echo "* * * * * /usr/bin/true" | crontab -
  touch "$1"/kbf-ci-owned
  mkdir -p "$1"/kbf-ci-dir/sub
  touch "$1"/kbf-ci-dir/sub/f
  ln -s /private/var/kbf-ci-sentinel "$1"/kbf-ci-link
  touch /private/tmp/kbf-ci-locked
  chflags uchg /private/tmp/kbf-ci-locked
  touch "$(getconf DARWIN_USER_TEMP_DIR)kbf-ci-temp"
  /bin/sleep 600 </dev/null >/dev/null 2>&1 &
' sh "$shared"
sudo -n test -e /private/var/at/tabs/kbf-lease-1-1 || fail "no crontab was planted"
[ "$(sudo -n find /private/var/folders -user "$first" -name kbf-ci-temp | wc -l)" -eq 1 ] || fail "no temp file"
pgrep -U "$first" sleep || fail "no sleep process"

step "user-delete is refused while a process of the uid remains"
expect_refused "processes of uid $first remain" "$client" delete "$socket" 1.1
id kbf-lease-1-1

step "kill-uid ends every process of the uid"
"$client" kill "$socket" 1.1
if pgrep -U "$first"; then fail "processes of $first survived kill-uid"; fi

step "user-delete removes the user and sweeps its state, never following a link"
[ "$("$client" delete "$socket" 1.1)" = existed ] || fail "delete"
if id kbf-lease-1-1 2>/dev/null; then fail "the user record survived"; fi
for gone in "$users"/kbf-lease-1-1 /private/var/at/tabs/kbf-lease-1-1 "$shared"/kbf-ci-owned \
  "$shared"/kbf-ci-dir "$shared"/kbf-ci-link /private/tmp/kbf-ci-locked $override; do
  if sudo -n test -e "$gone" || sudo -n test -L "$gone"; then fail "$gone survived the sweep"; fi
done
[ "$(sudo -n find /private/var/folders -user "$first" | wc -l)" -eq 0 ] || fail "files of $first remain in /private/var/folders"
sudo -n test -e /private/var/kbf-ci-sentinel/keep || fail "the sweep followed the planted link"
[ "$("$client" delete "$socket" 1.1)" = absent ] || fail "a repeated delete"

step "a lease id, and so a name, is never used twice"
expect_refused "never reused" "$client" create "$socket" 1.1
expect_refused "was deleted" "$client" kill "$socket" 1.1

step "an administrator only with a valid gate grant, used once"
uid=$("$client" create "$socket" 1.2)
[ "$uid" = $((first + 1)) ] || fail "uid $uid after $first"
if dseditgroup -o checkmember -m kbf-lease-1-2 admin; then fail "kbf-lease-1-2 is an administrator"; fi
now=$(date +%s)
grant=$("$client" grant 07 "$serial" 1.3 "$now")
"$client" create "$socket" 1.3 "$grant"
dseditgroup -o checkmember -m kbf-lease-1-3 admin || fail "kbf-lease-1-3 is not an administrator"
expect_refused "names lease 1.3" "$client" create "$socket" 1.4 "$grant"
expect_refused "never reused" "$client" create "$socket" 1.3 "$grant"
forged=$("$client" grant 08 "$serial" 1.4 "$now")
expect_refused "does not verify" "$client" create "$socket" 1.4 "$forged"
elsewhere=$("$client" grant 07 OTHERSERIAL 1.4 "$now")
expect_refused "serial" "$client" create "$socket" 1.4 "$elsewhere"
for lease in 1.2 1.3; do
  "$client" kill "$socket" "$lease"
  [ "$("$client" delete "$socket" "$lease")" = existed ] || fail "delete $lease"
done
if dscl . -read /Groups/admin GroupMembership | grep -q kbf-lease; then fail "a lease user is still in the admin group"; fi

step "the ledger survives a restart of the helper"
sudo -n pkill -f "$helper --socket"
sleep 1
sudo -n "$helper" --socket "$socket" --socket-group admin --state-dir "$state" \
  --uid-range "$first-$last" --daemon-requirement "cdhash H\"$genuine\"" \
  >> "$tmp/helper.log" 2>&1 &
# Up once it answers: the deleted lease 1.1 is "absent" from the ledger it reread.
for _ in $(seq 100); do
  [ "$("$client" delete "$socket" 1.1 2>/dev/null)" = absent ] && break
  sleep 0.1
done
expect_refused "never reused" "$client" create "$socket" 1.3
uid=$("$client" create "$socket" 1.5)
[ "$uid" = $((first + 3)) ] || fail "uid $uid after a restart, want $((first + 3))"
"$client" kill "$socket" 1.5
"$client" delete "$socket" 1.5

echo
echo "kbf-mac-session: every check passed"
