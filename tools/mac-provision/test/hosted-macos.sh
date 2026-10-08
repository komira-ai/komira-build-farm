#!/bin/sh
# Runs kbf-mac-provision on a hosted macOS runner, as root (the runner has
# passwordless sudo):
#
#   sudo sh tools/mac-provision/test/hosted-macos.sh [KBF_DAEMON]
#
# A hosted runner is a virtual machine that later steps and other jobs never reuse,
# but this script still applies only what cannot break the job running it
# (docs/design/mac-node-provisioning.md, section 10, lists what a hosted VM can
# prove). It writes a profile whose check-only values (macOS, FileVault, the Xcodes,
# the default developer directory) are read from this runner, so those keys must pass
# as they are. Then:
#
#   applied and asserted: the names, the role account, the software-update and App
#     Store preferences, the sshd drop-in, the firewall, the LaunchDaemon (running
#     KBF_DAEMON if given), and the default developer directory when xcode-select
#     points through a link: apply, a second apply that prints nothing, check passes;
#     two settings drifted by hand fail check by name and the next apply fixes them;
#     restart truncates the error log;
#   asserted as read: macos_version, macos_build, filevault, xcode, probe, and
#     simulator_runtimes failing exactly when `xcrun simctl list runtimes` lists one;
#   applied, printed, not asserted (a VM may accept and ignore pmset): power_*;
#   read and printed only: admin_user, remote_login, autologin, time_server,
#     time_max_offset_ms (Remote Login needs Full Disk Access; the clock and the
#     runner's own login are left alone).
#
# Every check line goes to the job summary when GITHUB_STEP_SUMMARY is set.

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT=$HERE/../kbf-mac-provision
DAEMON=${1-}
WORK=$(mktemp -d "${RUNNER_TEMP:-/private/tmp}/kbf-mac-provision-hosted.XXXXXX")
P=$WORK/profile
OUT=$WORK/out
FAILED=0
LABEL=org.example.kbf-daemon
SAFE=hostname,role_user,updates_auto_download,updates_auto_install_macos,updates_auto_install_security,updates_config_data,appstore_auto_update,ssh_password_auth,firewall,launchd_label,default_developer_dir
READ=macos_version,macos_build,filevault,xcode,default_developer_dir,probe

fail() {
  printf 'FAILED: %s\n' "$*"
  FAILED=$((FAILED + 1))
}

prov() {
  sh "$SCRIPT" "$@" >"$OUT" 2>&1
  STATUS=$?
  sed 's/^/  | /' "$OUT"
}

want_status() {
  [ "$STATUS" = "$1" ] || fail "$2: exit status $STATUS, want $1"
}

want() {
  grep -qF -- "$1" "$OUT" || fail "output lacks: $1"
}

[ "$(id -u)" = 0 ] || {
  echo "hosted-macos.sh runs as root (sudo)" >&2
  exit 2
}

# ---------------------------------------------------------------- the profile

role_id=480
while dscl . -list /Users UniqueID | awk -v id="$role_id" '$2 == id { f = 1 } END { exit !f }'; do
  role_id=$((role_id + 1))
done
filevault=off
case "$(fdesetup status | head -n 1)" in
  'FileVault is On.') filevault=on ;;
esac
echo 'echo identity' >"$WORK/probe.sh"
default_dir=$(xcode-select -p)
default_app=${default_dir%/Contents/Developer}
if [ -L "$default_app" ]; then
  default_dir="$(cd -P "$default_app" && pwd)/Contents/Developer"
fi
{
  sed -e '/^#/d' -e '/^xcode/d' -e '/^probe=/d' -e '/^expect_probe=/d' -e '/^default_developer_dir=/d' \
    -e "s/^macos_version=.*/macos_version=$(sw_vers -productVersion)/" \
    -e "s/^macos_build=.*/macos_build=$(sw_vers -buildVersion)/" \
    -e 's/^hostname=.*/hostname=kbf-mac-01/' \
    -e "s/^admin_user=.*/admin_user=${SUDO_USER:-runner}/" \
    -e "s/^role_id=.*/role_id=$role_id/" \
    -e "s/^filevault=.*/filevault=$filevault/" \
    -e 's/^time_server=.*/time_server=time.apple.com/' \
    "$HERE/../example.profile"
  builds=' '
  for app in /Applications/Xcode*.app; do
    [ -d "$app" ] || continue
    [ -L "$app" ] && continue
    b=$(/usr/libexec/PlistBuddy -c 'Print :ProductBuildVersion' "$app/Contents/version.plist")
    case "$builds" in
      *" $b "*) echo "note: $app repeats build $b; it shows as unpinned" >&2 ;;
      *)
        builds="$builds$b "
        echo "xcode=$b:$app/Contents/Developer:$(printf '%064d' 0)"
        ;;
    esac
  done
  echo "default_developer_dir=$default_dir"
  echo "probe=host_identity:$WORK/probe.sh:$(shasum -a 256 "$WORK/probe.sh" | cut -d ' ' -f 1)"
} >"$P"
echo "== profile"
sed 's/^/  | /' "$P"

if [ -n "$DAEMON" ]; then
  mkdir -p /usr/local/kbf/current
  cp "$DAEMON" /usr/local/kbf/current/kbf-daemon
  chmod 755 /usr/local/kbf/current/kbf-daemon
fi

# ---------------------------------------------------------------- read as they are

echo "== print"
prov print --profile "$P"
want_status 0 print
want "node_label=profile=mac-arm64-pool@"

echo "== check, before apply (every item)"
prov check --profile "$P"
cp "$OUT" "$WORK/before"
for k in $(echo "$READ" | tr ',' ' '); do
  [ "$k" = default_developer_dir ] && continue
  want "PASS $k:"
done
echo "== simctl list runtimes"
runtimes=$(xcrun simctl list runtimes 2>/dev/null | grep -c '^[a-zA-Z].* (' || true)
xcrun simctl list runtimes 2>&1 | sed 's/^/  | /'
if [ "$runtimes" -gt 0 ]; then
  want "FAIL simulator_runtimes:"
else
  want "PASS simulator_runtimes:"
fi

# ---------------------------------------------------------------- applied

echo "== apply power (not asserted: a VM may ignore pmset)"
prov apply --profile "$P" --keys power_sleep,power_standby,power_powernap,power_autorestart,power_womp

echo "== apply $SAFE"
prov apply --profile "$P" --keys "$SAFE"
want_status 0 "first apply"
want "CHANGE launchd_label:"
want "CHANGE role_user:"
echo "== apply again: prints nothing"
prov apply --profile "$P" --keys "$SAFE"
want_status 0 "second apply"
[ ! -s "$OUT" ] || fail "the second apply printed something"
prov check --profile "$P" --keys "$SAFE"
want_status 0 "check after apply"
launchctl print "system/$LABEL" >/dev/null || fail "system/$LABEL is not loaded"

echo "== drift by hand: AutomaticDownload and ComputerName"
defaults write /Library/Preferences/com.apple.SoftwareUpdate AutomaticDownload -bool true
scutil --set ComputerName kbf-drifted
prov check --profile "$P" --keys "$SAFE"
want_status 1 "check after drift"
want "FAIL updates_auto_download: AutomaticDownload is 1, want 0"
want "FAIL hostname: want kbf-mac-01, found ComputerName=kbf-drifted"
[ "$(grep -c '^FAIL' "$OUT")" = 2 ] || fail "want exactly two FAIL lines"
prov apply --profile "$P" --keys "$SAFE"
want_status 0 "apply after drift"
want "CHANGE updates_auto_download:"
want "CHANGE hostname: ComputerName = kbf-mac-01"

echo "== restart truncates the error log"
mkdir -p /Library/Logs/kbf
echo 'kbf-hosted-marker' >>/Library/Logs/kbf/kbf-daemon.err.log
prov restart --profile "$P"
want_status 0 restart
if grep -q kbf-hosted-marker /Library/Logs/kbf/kbf-daemon.err.log; then fail "the error log was not truncated"; fi
launchctl print "system/$LABEL" >/dev/null || fail "system/$LABEL is not loaded after restart"

echo "== check, after apply (every item)"
prov check --profile "$P"
if [ -n "${GITHUB_STEP_SUMMARY-}" ]; then
  {
    echo '### kbf-mac-provision on a hosted macOS runner'
    echo
    echo "macOS $(sw_vers -productVersion) ($(sw_vers -buildVersion)). Applied and asserted: \`$SAFE\`."
    echo 'Asserted as read: macos_version, macos_build, filevault, xcode, probe, simulator_runtimes.'
    echo 'Power keys applied, not asserted; admin_user, remote_login, autologin and time read only.'
    echo
    echo '```text'
    echo '# check before apply'
    cat "$WORK/before"
    echo '# check after apply'
    cat "$OUT"
    echo '```'
  } >>"$GITHUB_STEP_SUMMARY"
fi

launchctl bootout "system/$LABEL" 2>/dev/null
echo "hosted-macos: $FAILED failed assertions"
[ "$FAILED" = 0 ]
