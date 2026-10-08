#!/bin/sh
# Tests kbf-mac-provision on Linux (or any POSIX host) against a fake Mac: a fake root
# holding a fake of each macOS program the script runs (fakecmd), whose state lives in
# files the tests set and read.
#
#   sh tools/mac-provision/test/run.sh [SCRIPT]
#
# SCRIPT defaults to tools/mac-provision/kbf-mac-provision; mutants.sh passes a
# mutated copy. Environment: KBF_SH, the shell that runs the script (default sh);
# KBF_TRACE, a file: the script then runs under `bash -x` and its trace is appended
# there for coverage.awk; KBF_TESTS, the tests to run (default all); KBF_DRIFT, the
# items whose drift cases t_drift runs (default all). Prints one line
# per failed assertion and a summary; exits 1 if any test failed.

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT=${1:-$HERE/../kbf-mac-provision}
SCRIPT=$(cd "$(dirname "$SCRIPT")" && pwd)/${SCRIPT##*/}
EXAMPLE=$HERE/../example.profile
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-mac-provision-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAKE_REAL_PATH=$PATH
export FAKE_REAL_PATH
TESTS=0
FAILED=0
NAME=
OUT=$WORK/out
ERR=$WORK/err
P=$WORK/profile
: >"$OUT"
: >"$ERR"

# ---------------------------------------------------------------- harness

failt() {
  FAILED=$((FAILED + 1))
  printf 'not ok %s: %s\n' "$NAME" "$*"
  sed -n '1,40s/^/    out: /p' "$OUT"
  sed -n '1,10s/^/    err: /p' "$ERR"
}

# Runs the script with its arguments and --root of the current fake Mac.
run() {
  if [ -n "${KBF_TRACE-}" ]; then
    PS4='+@${LINENO}@ ' BASH_XTRACEFD=9 bash -x "$SCRIPT" "$@" >"$OUT" 2>"$ERR" 9>>"$KBF_TRACE"
  else
    ${KBF_SH:-sh} "$SCRIPT" "$@" >"$OUT" 2>"$ERR"
  fi
  STATUS=$?
}

runr() {
  run "$@" --root "$FAKE_ROOT"
}

want_status() {
  [ "$STATUS" = "$1" ] || failt "exit status $STATUS, want $1"
}

want_out() {
  grep -qF -- "$1" "$OUT" || failt "stdout lacks: $1"
}

lacks_out() {
  if grep -qF -- "$1" "$OUT"; then failt "stdout has: $1"; fi
}

want_err() {
  grep -qF -- "$1" "$ERR" || failt "stderr lacks: $1"
}

want_lines() {
  wl_n=$(grep -c -- "$1" "$OUT")
  [ "$wl_n" = "$2" ] || failt "$wl_n lines match $1, want $2"
}

want_file() {
  grep -qF -- "$2" "$1" 2>/dev/null || failt "$1 lacks: $2"
}

# The calls that change the fake Mac, from its log.
writes() {
  grep -E '^(pmset -a|defaults (write|delete)|scutil --set|systemsetup (-f|-set)|socketfilterfw --set|dscl \. -(create|delete)|launchctl boot|xcode-select -s|sntp -sS|xip |chown |mv |fdesetup [^s])' "$S/log"
}

no_writes() {
  if writes >/dev/null; then failt "changed the Mac: $(writes | head -n 3 | tr '\n' ';')"; fi
}

# ---------------------------------------------------------------- the fake Mac

kv() {
  # kv FILE KEY VALUE: set KEY in a fake state file.
  if [ -f "$1" ]; then grep -v "^$2=" "$1" >"$1.tmp"; else : >"$1.tmp"; fi
  printf '%s=%s\n' "$2" "$3" >>"$1.tmp"
  mv "$1.tmp" "$1"
}

user() {
  # user NAME UID: a local account in the fake directory.
  kv "$S/dscl_Users_$1" UniqueID "$2"
}

# A Mac as Setup Assistant and Remote Login leave it: one admin, nothing else set.
ROOTS=0

mkroot() {
  ROOTS=$((ROOTS + 1))
  FAKE_ROOT=$WORK/root$ROOTS
  export FAKE_ROOT
  S=$FAKE_ROOT/.fake
  for d in .fake usr/bin bin usr/sbin sbin usr/libexec/ApplicationFirewall Library/Preferences \
    etc/ssh/sshd_config.d Applications Library/kbf/xip usr/local/kbf/probes; do
    mkdir -p "$FAKE_ROOT/$d"
  done
  for c in id sw_vers pmset defaults dscl launchctl xcode-select xcodebuild sntp xip shasum ls mv chown fdesetup; do
    ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/bin/$c"
  done
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/sbin/scutil"
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/sbin/systemsetup"
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/sbin/dseditgroup"
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/sbin/sshd"
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/libexec/PlistBuddy"
  ln -s "$HERE/fakecmd" "$FAKE_ROOT/usr/libexec/ApplicationFirewall/socketfilterfw"
  for t in awk sed grep cat mkdir rm chmod tr cut head tail sleep sort; do
    ln -s "$(command -v "$t")" "$FAKE_ROOT/bin/$t"
  done
  : >"$S/log"
  echo 0 >"$S/uid"
  printf 'version=27.0.1\nbuild=26A000\n' >"$S/sw_vers"
  printf 'sleep=1\nstandby=1\npowernap=1\nautorestart=0\nwomp=0\n' >"$S/pmset"
  printf 'ComputerName=Farm Admin Mac\nLocalHostName=Farm-Admin-Mac\n' >"$S/scutil"
  printf 'remotelogin=on\nnetworktime=on\ntimeserver=time.apple.com\n' >"$S/systemsetup"
  echo 0 >"$S/firewall"
  echo Off >"$S/filevault"
  echo +0.004000 >"$S/sntp_offset"
  user root 0
  user farmadmin 501
  kv "$S/dscl_Groups_admin" GroupMembership 'root farmadmin'
  printf '# sshd_config\nInclude /etc/ssh/sshd_config.d/*\n' >"$FAKE_ROOT/etc/ssh/sshd_config"
  echo 'app=Xcode-26.6.app build=17A000' >"$FAKE_ROOT/Library/kbf/xip/Xcode_26.6.xip"
  echo 'app=Xcode-27.0.app build=18A000' >"$FAKE_ROOT/Library/kbf/xip/Xcode_27.0.xip"
  echo 'echo identity' >"$FAKE_ROOT/usr/local/kbf/probes/host_identity.sh"
}

sha() {
  sha256sum "$1" | cut -d ' ' -f 1
}

# The test profile, matching the fake Mac's xips and probe; each argument is a sed
# command applied to it.
mkprofile() {
  {
    sed -e '/^#/d' -e '/^xcode=/d' -e '/^probe=/d' -e 's/^profile=.*/profile=mac-arm64-test/' "$EXAMPLE"
    echo "xcode=17A000:/Applications/Xcode-26.6.app/Contents/Developer:$(sha "$FAKE_ROOT/Library/kbf/xip/Xcode_26.6.xip")"
    echo "xcode=18A000:/Applications/Xcode-27.0.app/Contents/Developer:$(sha "$FAKE_ROOT/Library/kbf/xip/Xcode_27.0.xip")"
    echo "probe=host_identity:/usr/local/kbf/probes/host_identity.sh:$(sha "$FAKE_ROOT/usr/local/kbf/probes/host_identity.sh")"
  } >"$P.base"
  cp "$P.base" "$P"
  for mp_e in "$@"; do
    sed -e "$mp_e" "$P" >"$P.tmp"
    mv "$P.tmp" "$P"
  done
}

# A fake Mac with the test profile applied.
converged() {
  mkroot
  mkprofile
  runr apply --profile "$P"
  if [ "$STATUS" != 0 ]; then failt "the first apply failed"; fi
  : >"$S/log"
}

PLIST_REL=Library/LaunchDaemons/org.example.kbf-daemon.plist

# ---------------------------------------------------------------- tests

t_usage() {
  mkroot
  mkprofile
  for tu in '' 'frobnicate' 'check' 'check --profile' 'check --profile x --bogus y' 'check --profile x --root relative'; do
    # shellcheck disable=SC2086 # each case is a word list
    run $tu
    want_status 2
  done
  want_err 'absolute path'
  run check --profile "$WORK/missing" --root "$FAKE_ROOT"
  want_status 2
  want_err 'no profile file'
}

t_print() {
  mkroot
  mkprofile
  runr print --profile "$P"
  want_status 0
  want_out "profile_sha256=$(sha "$P")"
  want_out "node_label=profile=mac-arm64-test@$(sha "$P" | cut -c 1-12)"
  want_out 'hostname=kbf-mac-07'
  want_out 'xcode_xip_dir=/Library/kbf/xip'
  want_out 'kbf_labels=pool=mac rack=r2'
  want_out 'xcode=17A000:/Applications/Xcode-26.6.app/Contents/Developer:'
  want_out 'probe=host_identity:/usr/local/kbf/probes/host_identity.sh:'
  want_out 'expect_probe=host_identity:18A000='
  lacks_out '#'
  no_writes
  # Without xcode_xip_dir, print leaves the optional key out.
  mkprofile '/^xcode_xip_dir=/d'
  runr print --profile "$P"
  want_status 0
  lacks_out 'xcode_xip_dir'
}

t_print_real_root() {
  # The default root, /: print reads only the profile.
  mkroot
  mkprofile
  run print --profile "$EXAMPLE"
  want_status 0
  want_out 'node_label=profile=mac-arm64-pool@'
}

# refused DESCRIPTION STDERR SED...: the profile edited by SED is refused with STDERR.
refused() {
  rf_name=$1
  rf_err=$2
  shift 2
  mkprofile "$@"
  runr print --profile "$P"
  if [ "$STATUS" != 2 ]; then failt "$rf_name: exit status $STATUS, want 2"; fi
  grep -qF -- "$rf_err" "$ERR" || failt "$rf_name: stderr lacks: $rf_err"
}

t_refused() {
  mkroot
  cd "$WORK" || exit 1
  refused 'unknown key' 'unknown key: sleep' '$a\
sleep=0'
  refused 'duplicate key' 'duplicate key: firewall' '$a\
firewall=on'
  # A key with a space would match two adjacent known keys and reach eval as a command.
  refused 'key with a space' 'unknown key: default_developer_dir xcode_xip_dir' \
    '/^xcode=/d' '/^expect_probe=/d' '/^default_developer_dir=/d' '/^xcode_xip_dir=/d' '$a\
default_developer_dir xcode_xip_dir=/x'
  refused 'not KEY=VALUE' 'not KEY=VALUE' '$a\
justtext'
  refused 'missing key' 'missing key: firewall' '/^firewall=/d'
  refused 'command substitution' 'may hold only' 's/^hostname=.*/hostname=$(mkdir PWNED)kbf-mac-07/'
  refused 'backquote' 'may hold only' 's/^hostname=.*/hostname=`mkdir PWNED`/'
  refused 'semicolon' 'may hold only' 's/^time_server=.*/time_server=a;mkdir PWNED/'
  [ ! -e "$WORK/PWNED" ] || failt "a profile value was run as a command"
  refused 'bad profile name' 'profile: ' 's/^profile=.*/profile=Mac Pool/'
  refused 'bad version' 'macos_version: ' 's/^macos_version=.*/macos_version=27.x/'
  refused 'bad build' 'macos_build: ' 's/^macos_build=.*/macos_build=26a000/'
  refused 'upper-case name' 'hostname: ' 's/^hostname=.*/hostname=Kbf-Mac-07/'
  refused 'name with a space' 'hostname: ' 's/^hostname=.*/hostname=kbf mac 07/'
  refused 'name without -mac-' 'hostname: ' 's/^hostname=.*/hostname=kbf-07/'
  refused 'long name' 'longer than 63' "s/^hostname=.*/hostname=$(printf '%060d' 0 | tr 0 a)-mac-1/"
  refused 'bad admin' 'admin_user: ' 's/^admin_user=.*/admin_user=Admin/'
  refused 'role without _' 'role_user: ' 's/^role_user=.*/role_user=kbf/'
  refused 'role id text' 'role_id: ' 's/^role_id=.*/role_id=x/'
  refused 'role id low' 'below 200' 's/^role_id=.*/role_id=150/'
  refused 'role id high' 'below 500' 's/^role_id=.*/role_id=501/'
  refused 'lease range text' 'lease_uids: ' 's/^lease_uids=.*/lease_uids=600/'
  refused 'lease range low' 'starts below 501' 's/^lease_uids=.*/lease_uids=400-699/'
  refused 'lease range empty' 'is empty' 's/^lease_uids=.*/lease_uids=700-600/'
  refused 'sleep text' 'power_sleep: ' 's/^power_sleep=.*/power_sleep=never/'
  refused 'autorestart 2' 'power_autorestart: ' 's/^power_autorestart=.*/power_autorestart=2/'
  refused 'firewall maybe' 'firewall: ' 's/^firewall=.*/firewall=maybe/'
  refused 'filevault x' 'filevault: ' 's/^filevault=.*/filevault=x/'
  refused 'autologin user' 'autologin: ' 's/^autologin=.*/autologin=farmadmin/'
  refused 'simulator runtimes' 'simulator_runtimes: ' 's/^simulator_runtimes=.*/simulator_runtimes=ios/'
  refused 'time server' 'time_server: ' 's/^time_server=.*/time_server=-x/'
  refused 'label' 'launchd_label: ' 's/^launchd_label=.*/launchd_label=.x/'
  refused 'plain http server' 'kbf_server: ' 's|^kbf_server=.*|kbf_server=http://farm.example.net:8981|'
  refused 'cas with a path' 'kbf_cas: ' 's|^kbf_cas=.*|kbf_cas=https://farm.example.net/x|'
  refused 'relative cert' 'kbf_cert: ' 's|^kbf_cert=.*|kbf_cert=node.pem|'
  refused 'bad label' 'is not key=value' 's/^kbf_labels=.*/kbf_labels=pool=mac rack/'
  refused 'profile label' 'profile label' 's/^kbf_labels=.*/kbf_labels=profile=x/'
  refused 'relative xip dir' 'xcode_xip_dir: ' 's|^xcode_xip_dir=.*|xcode_xip_dir=xip|'
  refused 'xcode outside /Applications' 'xcode: ' 's|^xcode=17A000:/Applications|xcode=17A000:/opt/x|'
  refused 'xcode twice' 'pinned twice' 's|^xcode=17A000|xcode=18A000|'
  refused 'default dir not pinned' 'default_developer_dir: ' 's|^default_developer_dir=.*|default_developer_dir=/Applications/Xcode.app/Contents/Developer|'
  refused 'default dir without xcode' 'no xcode is pinned' '/^xcode=/d' '/^expect_probe=/d'
  refused 'bad probe' 'probe: ' 's|^probe=host_identity:|probe=Host:|'
  refused 'probe twice' 'named twice' '$a\
probe=host_identity:/x:0000000000000000000000000000000000000000000000000000000000000000'
  refused 'bad expectation' 'expect_probe: ' 's/^expect_probe=host_identity:17A000=.*/expect_probe=host_identity=1/'
  refused 'expectation of no probe' 'names no probe' 's/^expect_probe=host_identity:17A000/expect_probe=other:17A000/'
  refused 'expectation of no xcode' 'names no pinned xcode' 's/^expect_probe=host_identity:17A000/expect_probe=host_identity:19A000/'
  refused 'expectation twice' 'host_identity:18A000 is named twice' '$a\
expect_probe=host_identity:18A000=x'
  printf 'profile=x' >"$P"
  runr print --profile "$P"
  want_status 2
  want_err 'no newline'
  [ ! -e "$WORK/PWNED" ] || failt "a profile value was run as a command"
}

t_converge() {
  mkroot
  mkprofile
  runr check --profile "$P"
  want_status 1
  for tc in hostname role_user power_sleep power_standby power_powernap power_autorestart power_womp \
    updates_auto_download updates_auto_install_macos updates_auto_install_security updates_config_data \
    appstore_auto_update ssh_password_auth firewall time_server xcode default_developer_dir launchd_label; do
    want_out "FAIL $tc:"
  done
  want_out 'check: 9 pass, 18 fail'
  no_writes
  runr apply --profile "$P"
  want_status 0
  for tc in 'hostname: HostName = kbf-mac-07' 'role_user: role account' 'power_sleep: pmset -a sleep 0' \
    'updates_auto_download: AutomaticDownload = 0' 'updates_config_data: ConfigDataInstall = 1' \
    'ssh_password_auth: wrote' 'firewall: firewall on' 'time_server: network time on' \
    'xcode: installed Xcode 17A000' 'xcode: installed Xcode 18A000' 'default_developer_dir: xcode-select -s' \
    'launchd_label: wrote'; do
    want_out "CHANGE $tc"
  done
  lacks_out 'FAIL'
  want_file "$S/log" 'xcodebuild -license accept'
  want_file "$S/log" 'xcodebuild -runFirstLaunch'
  want_file "$FAKE_ROOT/etc/ssh/sshd_config.d/000-kbf.conf" 'PasswordAuthentication no'
  want_file "$FAKE_ROOT/etc/ssh/sshd_config.d/000-kbf.conf" 'KbdInteractiveAuthentication no'
  want_file "$FAKE_ROOT/etc/ssh/sshd_config.d/000-kbf.conf" 'PermitRootLogin no'
  pl=$FAKE_ROOT/$PLIST_REL
  for tc in '<string>org.example.kbf-daemon</string>' '<string>/usr/local/kbf/current/kbf-daemon</string>' \
    '<string>--driver</string>' '<string>native</string>' '<string>https://farm.example.net:8981</string>' \
    '<string>--node-id</string>' '<string>kbf-mac-07</string>' '<string>/Volumes/kbf/scratch</string>' \
    "<string>profile=mac-arm64-test@$(sha "$P" | cut -c 1-12)</string>" '<string>pool=mac</string>' \
    '<string>rack=r2</string>' '<key>UserName</key>' '<string>_kbf</string>' '<key>ProcessType</key>' \
    '<string>Interactive</string>' '<key>ExitTimeOut</key>' '<integer>65536</integer>' \
    '<string>/Library/Logs/kbf/kbf-daemon.err.log</string>'; do
    want_file "$pl" "$tc"
  done
  # The second apply finds nothing to do, and says nothing.
  : >"$S/log"
  runr apply --profile "$P"
  want_status 0
  [ ! -s "$OUT" ] || failt "the second apply printed something"
  no_writes
  runr check --profile "$P"
  want_status 0
  want_out 'check: 27 pass, 0 fail'
  want_out 'PASS updates_auto_download: AutomaticDownload = 0 (the SoftwareUpdate payload is removed in macOS 27'
  want_out 'PASS filevault: off'
  want_out 'PASS role_user: _kbf (uid and gid 480, hidden, no shell, no password)'
  want_out 'PASS autologin: off'
}

# drift ITEM FIXED COMMAND [TEXT]: on a converged Mac, COMMAND (shell, with $S and $R)
# makes exactly ITEM fail, and check prints TEXT; apply then fixes it (FIXED = fix) or
# leaves it, changing nothing on the Mac (stay; FileVault is never touched). Afterwards
# $OUT holds what apply printed.
drift() {
  case " ${KBF_DRIFT:-$1} " in
    *" $1 "*) : ;;
    *) return 0 ;;
  esac
  converged
  # shellcheck disable=SC2034 # R is read by the drift command
  R=$FAKE_ROOT
  eval "$3"
  NAME="drift $1 ($3)"
  runr check --profile "$P"
  want_status 1
  want_out "FAIL $1:"
  want_lines '^FAIL' 1
  [ -z "${4-}" ] || want_out "$4"
  : >"$S/log"
  runr apply --profile "$P"
  if [ "$2" = fix ]; then
    want_status 0
    want_out "CHANGE $1:"
    cp "$OUT" "$WORK/applied"
    runr check --profile "$P"
    want_status 0
    cp "$WORK/applied" "$OUT"
  else
    want_status 1
    want_out "FAIL $1:"
    want_lines '^FAIL' 1
    no_writes
  fi
}

t_drift() {
  drift macos_version stay 'kv $S/sw_vers version 27.0.2'
  want_out '[check only'
  drift macos_build stay 'kv $S/sw_vers build 26A001'
  drift hostname fix 'kv $S/scutil ComputerName other'
  drift admin_user stay 'kv $S/dscl_Groups_admin GroupMembership "root farmadmin intruder"'
  want_out 'other administrators: intruder'
  drift admin_user stay 'kv $S/dscl_Groups_admin GroupMembership root'
  drift admin_user stay 'rm $S/dscl_Users_farmadmin'
  # An administrator by primary group (80, admin), listed in no GroupMembership.
  drift admin_user stay 'user hidden 502; kv $S/dscl_Users_hidden PrimaryGroupID 80'
  want_out 'other administrators: hidden'
  drift role_user fix 'kv $S/dscl_Users__kbf UserShell /bin/zsh'
  # The role account has no password: Password is *, and no AuthenticationAuthority
  # (which dscl prints on a line of its own) names a password hash.
  drift role_user fix 'kv $S/dscl_Users__kbf Password "********"'
  drift role_user fix 'kv $S/dscl_Users__kbf AuthenticationAuthority ";ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2>"' 'AuthenticationAuthority=present'
  drift role_user stay 'user other 480'
  want_out 'uid 480 belongs to other'
  drift role_user stay 'kv $S/dscl_Groups_other PrimaryGroupID 480'
  want_out 'gid 480 belongs to other'
  for dp in sleep standby powernap; do
    drift "power_$dp" fix "kv \$S/pmset $dp 1"
  done
  drift power_autorestart fix 'kv $S/pmset autorestart 0'
  drift power_womp fix 'kv $S/pmset womp 0'
  drift power_sleep fix 'kv $S/pmset sleep ""' 'sleep is not reported'
  drift updates_auto_download fix 'kv $S/defaults_Library_Preferences_com.apple.SoftwareUpdate AutomaticDownload 1'
  drift updates_auto_install_macos fix 'kv $S/defaults_Library_Preferences_com.apple.SoftwareUpdate AutomaticallyInstallMacOSUpdates 1'
  drift updates_auto_install_security fix 'kv $S/defaults_Library_Preferences_com.apple.SoftwareUpdate CriticalUpdateInstall 1'
  drift updates_config_data fix 'kv $S/defaults_Library_Preferences_com.apple.SoftwareUpdate ConfigDataInstall 0'
  drift appstore_auto_update fix 'kv $S/defaults_Library_Preferences_com.apple.SoftwareUpdate AutomaticallyInstallAppUpdates 1'
  drift remote_login fix 'kv $S/systemsetup remotelogin off'
  drift ssh_password_auth fix 'echo "PasswordAuthentication yes" >$R/etc/ssh/sshd_config.d/000-kbf.conf'
  drift ssh_password_auth fix 'echo "1000:1000 $R/etc/ssh/sshd_config.d/000-kbf.conf" >>$S/owners'
  drift ssh_password_auth stay 'echo "# no include" >$R/etc/ssh/sshd_config'
  # What sshd uses, not only the drop-in: a drop-in sorting first ("-" sorts before
  # "0"), a keyword before the Include line (any case, "=" or a space), and what
  # sshd -T reports.
  drift ssh_password_auth stay 'echo "PasswordAuthentication yes" >$R/etc/ssh/sshd_config.d/00-x.conf'
  want_out 'sshd reads 00-x.conf before 000-kbf.conf'
  drift ssh_password_auth stay 'printf "PasswordAuthentication yes\nInclude /etc/ssh/sshd_config.d/*\n" >$R/etc/ssh/sshd_config'
  want_out 'sets PasswordAuthentication before its Include line'
  drift ssh_password_auth stay 'printf "  permitRootLogin=yes\nInclude /etc/ssh/sshd_config.d/*\n" >$R/etc/ssh/sshd_config'
  want_out 'sets permitRootLogin=yes before its Include line'
  drift ssh_password_auth stay 'printf "passwordauthentication yes\nkbdinteractiveauthentication no\npermitrootlogin no\n" >$S/sshd_effective'
  want_out 'sshd -T reports'
  drift firewall fix 'echo 0 >$S/firewall'
  drift firewall fix 'echo broken >$S/firewall'
  drift filevault stay 'echo On >$S/filevault'
  drift filevault stay 'echo Decrypting >$S/filevault'
  want_out 'FileVault unknown (FileVault is Decrypting.)'
  drift autologin fix 'kv $S/defaults_Library_Preferences_com.apple.loginwindow autoLoginUser farmadmin; echo x >$R/etc/kcpassword'
  want_out 'CHANGE autologin: removed /etc/kcpassword'
  drift autologin fix 'kv $S/defaults_Library_Preferences_com.apple.loginwindow autoLoginUser farmadmin'
  drift autologin fix 'echo x >$R/etc/kcpassword'
  drift autologin fix 'user kbf-lease-9 700; kv $S/defaults_Library_Preferences_com.apple.loginwindow autoLoginUser kbf-lease-9'
  drift autologin fix 'user lease9 650; kv $S/defaults_Library_Preferences_com.apple.loginwindow autoLoginUser lease9'
  drift autologin fix 'kv $S/defaults_Library_Preferences_com.apple.loginwindow autoLoginUser kbf-lease-gone'
  drift time_server fix 'kv $S/systemsetup networktime off'
  drift time_server fix 'kv $S/systemsetup timeserver time.apple.com'
  drift time_max_offset_ms fix 'echo +1.500000 >$S/sntp_offset'
  drift time_max_offset_ms fix 'echo -0.900000 >$S/sntp_offset'
  drift time_max_offset_ms fix 'echo fail >$S/sntp_offset'
  drift xcode fix 'echo ProductBuildVersion=17A001 >$R/Applications/Xcode-26.6.app/Contents/version.plist'
  drift xcode fix 'rm -r $R/Applications/Xcode-27.0.app'
  drift xcode fix 'mkdir -p $R/Applications/Xcode-beta.app'
  want_out 'CHANGE xcode: removed /Applications/Xcode-beta.app'
  drift default_developer_dir fix 'echo /Applications/Xcode-26.6.app/Contents/Developer >$S/xcode_select'
  drift probe stay 'echo changed >>$R/usr/local/kbf/probes/host_identity.sh'
  drift simulator_runtimes stay 'mkdir -p $R/Library/Developer/CoreSimulator/Images; touch $R/Library/Developer/CoreSimulator/Images/x.dmg'
  drift simulator_runtimes stay 'mkdir -p $R/Library/Developer/CoreSimulator/Profiles/Runtimes/iOS.simruntime'
  drift simulator_runtimes stay 'mkdir -p $R/Library/Developer/CoreSimulator/Volumes/iOS_23A'
  want_out '/Library/Developer/CoreSimulator/Volumes/iOS_23A'
  drift launchd_label fix 'sed -i s/Interactive/Standard/ $R/$PLIST_REL'
  drift launchd_label fix 'echo "1000:1000 $R/$PLIST_REL" >>$S/owners'
  # The log directory is root's; the error log is the role account's, a regular file.
  drift launchd_label fix 'echo "480:480 $R/Library/Logs/kbf" >>$S/owners'
  drift launchd_label fix 'echo "0:0 $R/Library/Logs/kbf/kbf-daemon.err.log" >>$S/owners'
  drift launchd_label fix 'chmod 600 $R/Library/Logs/kbf/kbf-daemon.err.log'
  drift launchd_label fix 'rm $R/Library/Logs/kbf/kbf-daemon.err.log'
  drift launchd_label fix 'rm $S/loaded_system_org.example.kbf-daemon'
}

t_err_log() {
  # The role account owns the error log, and restart empties it as root. A link to a
  # root file planted in its place (a symbolic or a hard link) is never emptied:
  # check fails, restart refuses before it stops the job, and apply replaces the link.
  for el_ln in 'ln -s' ln; do
    converged
    el_log=$FAKE_ROOT/Library/Logs/kbf/kbf-daemon.err.log
    echo 'root: precious' >"$FAKE_ROOT/etc/precious"
    rm -f "$el_log"
    $el_ln "$FAKE_ROOT/etc/precious" "$el_log"
    NAME="t_err_log ($el_ln)"
    runr check --profile "$P" --keys launchd_label
    want_status 1
    want_out 'FAIL launchd_label: /Library/Logs/kbf/kbf-daemon.err.log is '
    runr restart --profile "$P"
    want_status 2
    want_err 'is not a regular file of _kbf'
    if grep -q '^launchctl boot' "$S/log"; then failt "restart touched the job"; fi
    runr apply --profile "$P"
    want_status 0
    want_out 'CHANGE launchd_label:'
    [ -s "$FAKE_ROOT/etc/precious" ] || failt "emptied the file a link points to"
    [ ! -L "$el_log" ] || failt "apply left the link"
    runr restart --profile "$P"
    want_status 0
  done
}

t_sshd_not_root() {
  # check as another user runs no sshd -T (it needs root), and says so.
  converged
  echo 501 >"$S/uid"
  runr check --profile "$P" --keys ssh_password_auth
  want_status 0
  want_out 'sshd -T not run: needs root'
  if grep -q '^sshd' "$S/log"; then failt "ran sshd -T"; fi
}

t_quiet_entries() {
  # Entries that are not drift: an images list beside the runtime images, a link to a
  # pinned Xcode, an unrelated file named like an xip.
  converged
  mkdir -p "$FAKE_ROOT/Library/Developer/CoreSimulator/Images"
  touch "$FAKE_ROOT/Library/Developer/CoreSimulator/Images/images.plist"
  ln -s "$FAKE_ROOT/Applications/Xcode-27.0.app" "$FAKE_ROOT/Applications/Xcode.app"
  # A commented keyword before the Include line, and a drop-in sorting after ours.
  printf '#PasswordAuthentication yes\nInclude /etc/ssh/sshd_config.d/*\n' >"$FAKE_ROOT/etc/ssh/sshd_config"
  echo 'PasswordAuthentication yes' >"$FAKE_ROOT/etc/ssh/sshd_config.d/100-macos.conf"
  runr check --profile "$P"
  want_status 0
}

t_lease_autologin() {
  # kbf-mac-session set auto-login to a lease user: check passes, apply leaves it.
  converged
  user kbf-lease-42 642
  kv "$S/defaults_Library_Preferences_com.apple.loginwindow" autoLoginUser kbf-lease-42
  echo x >"$FAKE_ROOT/etc/kcpassword"
  runr check --profile "$P"
  want_status 0
  want_out 'PASS autologin: lease user kbf-lease-42 (uid 642)'
  runr apply --profile "$P"
  want_status 0
  no_writes
  [ -e "$FAKE_ROOT/etc/kcpassword" ] || failt "apply removed a lease's kcpassword"
}

t_not_root() {
  mkroot
  mkprofile
  echo 501 >"$S/uid"
  runr apply --profile "$P"
  want_status 2
  want_err 'apply runs as root'
  runr restart --profile "$P"
  want_status 2
  want_err 'restart runs as root'
  no_writes
}

t_keys() {
  mkroot
  mkprofile
  runr check --profile "$P" --keys firewall,filevault
  want_status 1
  want_out 'FAIL firewall:'
  want_out 'PASS filevault:'
  want_out 'check: 1 pass, 1 fail'
  want_lines '^[PF]A' 2
  runr apply --profile "$P" --keys firewall
  want_status 0
  want_out 'CHANGE firewall:'
  want_lines '^CHANGE' 1
  runr check --profile "$P" --keys bogus
  want_status 2
  want_err 'unknown item: bogus'
}

t_other_values() {
  # The other value of the on/off keys: Remote Login off, passwords allowed.
  mkroot
  mkprofile 's/^remote_login=.*/remote_login=0/' 's/^ssh_password_auth=.*/ssh_password_auth=1/'
  runr apply --profile "$P" --keys remote_login,ssh_password_auth
  want_status 0
  want_out 'CHANGE remote_login: Remote Login off'
  want_file "$FAKE_ROOT/etc/ssh/sshd_config.d/000-kbf.conf" 'PasswordAuthentication yes'
  want_file "$FAKE_ROOT/etc/ssh/sshd_config.d/000-kbf.conf" 'PermitRootLogin no'
  runr check --profile "$P" --keys remote_login,ssh_password_auth
  want_status 0
  want_out 'PASS remote_login: Remote Login: Off'
}

t_restart() {
  converged
  mkdir -p "$FAKE_ROOT/Library/Logs/kbf"
  echo 'old panic' >"$FAKE_ROOT/Library/Logs/kbf/kbf-daemon.err.log"
  runr restart --profile "$P"
  want_status 0
  want_out 'CHANGE launchd_label: restarted system/org.example.kbf-daemon (error log truncated)'
  [ ! -s "$FAKE_ROOT/Library/Logs/kbf/kbf-daemon.err.log" ] || failt "the error log was not truncated"
  [ "$(grep -c '^launchctl boot' "$S/log")" = 2 ] || failt "want one bootout and one bootstrap"
  [ "$(grep '^launchctl boot' "$S/log" | head -n 1 | cut -d ' ' -f 2)" = bootout ] || failt "bootstrap before bootout"
  # A job that will not load: restart fails, and apply says so.
  touch "$S/bootstrap_fails"
  runr restart --profile "$P"
  want_status 2
  want_err 'launchctl bootstrap system'
  sed -i s/Interactive/Background/ "$FAKE_ROOT/$PLIST_REL"
  runr apply --profile "$P"
  want_status 1
  want_out 'the job did not restart'
}

t_xcode_sources() {
  # No xip directory: nothing to install from.
  mkroot
  mkprofile '/^xcode_xip_dir=/d'
  runr apply --profile "$P"
  want_status 1
  want_out 'NOTE xcode: no .xip with SHA-256'
  want_out 'FAIL xcode: /Applications/Xcode-26.6.app has no Xcode, want 17A000'
  # An xip that is not the pinned one is never expanded; a directory named *.xip is skipped.
  mkroot
  mkprofile
  echo 'app=Xcode-26.6.app build=17A000 tampered' >"$FAKE_ROOT/Library/kbf/xip/Xcode_26.6.xip"
  mkdir "$FAKE_ROOT/Library/kbf/xip/dir.xip"
  runr apply --profile "$P"
  want_status 1
  want_out 'NOTE xcode: no .xip with SHA-256'
  want_out 'CHANGE xcode: installed Xcode 18A000'
  if grep -q '^xip --expand .*Xcode_26.6.xip' "$S/log"; then failt "expanded an xip whose SHA-256 is not pinned"; fi
  # An xip with the pinned SHA-256 that holds another build is not installed.
  mkroot
  echo 'app=Xcode-26.6.app build=17A999' >"$FAKE_ROOT/Library/kbf/xip/Xcode_26.6.xip"
  mkprofile
  runr apply --profile "$P"
  want_status 1
  want_out 'did not expand to Xcode 17A000'
  [ ! -e "$FAKE_ROOT/Applications/.kbf-xip-17A000" ] || failt "left the staging directory"
  # No Xcode pinned: Xcodes found are drift, the default directory has nothing to say.
  mkroot
  mkprofile '/^xcode=/d' '/^expect_probe=/d' '/^default_developer_dir=/d'
  mkdir -p "$FAKE_ROOT/Applications/Xcode.app"
  runr check --profile "$P" --keys xcode,default_developer_dir
  want_status 1
  want_out 'FAIL xcode: /Applications/Xcode.app is not pinned'
  want_out 'PASS default_developer_dir: no Xcode is pinned'
}

# ---------------------------------------------------------------- main

ALL="t_usage t_print t_print_real_root t_refused t_converge t_drift t_quiet_entries t_lease_autologin t_not_root t_keys t_other_values t_restart t_err_log t_sshd_not_root t_xcode_sources"
for tn in ${KBF_TESTS:-$ALL}; do
  TESTS=$((TESTS + 1))
  NAME=$tn
  before=$FAILED
  "$tn"
  cd "$HERE" || exit 1
  if [ "$FAILED" = "$before" ]; then printf 'ok %s\n' "$tn"; fi
done
printf '%d tests, %d failed assertions\n' "$TESTS" "$FAILED"
[ "$FAILED" = 0 ]
