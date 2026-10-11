#!/bin/bash
# $1 = base profile file, $2 = lease dir
BASE=$(cat "$1"); L="$2"; mkdir -p "$L/home"
echo "== sw"; sw_vers; xcode-select -p
echo "== usbmuxd"; ls -la /var/run/usbmuxd /private/var/run/usbmuxd 2>&1
echo "== launchd plists"; grep -l -i -E "remoted|coredevice|usbmux|remotepairing" /System/Library/LaunchDaemons/*.plist /System/Library/LaunchAgents/*.plist /Library/LaunchDaemons/*.plist /Library/LaunchAgents/*.plist 2>/dev/null
for f in $(grep -l -i -E "remoted|coredevice|usbmux|remotepairing" /System/Library/LaunchDaemons/*.plist /System/Library/LaunchAgents/*.plist /Library/LaunchDaemons/*.plist /Library/LaunchAgents/*.plist 2>/dev/null); do echo "-- $f"; plutil -extract MachServices json -o - "$f" 2>&1; plutil -extract Sockets json -o - "$f" 2>&1 | head -c 600; echo; done
echo "== CoreDevice xpc"; for d in /Library/Developer/PrivateFrameworks/CoreDevice.framework /System/Library/PrivateFrameworks/CoreDevice.framework "$(xcode-select -p)/../SharedFrameworks/CoreDevice.framework"; do ls "$d/Versions/A/XPCServices" "$d/XPCServices" 2>/dev/null; done
find /Library/Developer/PrivateFrameworks /System/Library/PrivateFrameworks -maxdepth 4 -name "*.xpc" 2>/dev/null | grep -i -E "coredevice|remote" | while read x; do echo "$x $(plutil -extract CFBundleIdentifier raw -o - "$x/Contents/Info.plist" 2>&1)"; done
echo "== launchctl"; launchctl print system 2>/dev/null | grep -i -E "remote|coredevice|usbmux" | head -40
launchctl print gui/$(id -u) 2>/dev/null | grep -i -E "remote|coredevice|usbmux" | head -40
launchctl print user/$(id -u) 2>/dev/null | grep -i -E "remote|coredevice|usbmux" | head -40
echo "== pgrep"; ps -axo pid,user,comm | grep -i -E "remote|coredevice|usbmux" | grep -v grep
run() { # name, extra rules
  local name=$1 extra=$2
  local out; out=$(cd "$L" && HOME="$L/home" /usr/bin/sandbox-exec -D KBF_LEASE="$(cd "$L"; pwd -P)" -p "$BASE$extra" /usr/bin/xcrun devicectl list devices 2>&1); local rc=$?
  echo "-- devicectl[$name] rc=$rc"; echo "$out" | head -15
}
echo "== devicectl unsandboxed"; (cd "$L" && HOME="$L/home" /usr/bin/xcrun devicectl list devices 2>&1; echo "rc=$?") | head -20
echo "== devicectl unsandboxed real home"; (/usr/bin/xcrun devicectl list devices 2>&1; echo "rc=$?") | head -20
run base ""
run usbmux $'(deny network-outbound (literal "/private/var/run/usbmuxd"))\n'
run cd-global $'(deny mach-lookup (global-name-prefix "com.apple.CoreDevice"))\n'
run cd-xpc $'(deny mach-lookup (xpc-service-name-prefix "com.apple.CoreDevice"))\n'
run remoted $'(deny mach-lookup (global-name-prefix "com.apple.remoted"))\n'
run remote-all $'(deny mach-lookup (global-name-prefix "com.apple.remote"))\n'
run cd-dt $'(deny mach-lookup (global-name-prefix "com.apple.dt"))\n'
run allmach $'(deny mach-lookup)\n'
echo "== perl usbmuxd"
P='use Socket; socket(my $s, PF_UNIX, SOCK_STREAM, 0) or die "socket: $!"; if (connect($s, sockaddr_un("/var/run/usbmuxd"))) { print "connected\n" } else { print "$!\n" }'
echo "plain: $(perl -e "$P")"
echo "base: $(/usr/bin/sandbox-exec -D KBF_LEASE=/x -p "$BASE" perl -e "$P" 2>&1)"
echo "deny-literal: $(/usr/bin/sandbox-exec -D KBF_LEASE=/x -p "$BASE"$'(deny network-outbound (literal "/private/var/run/usbmuxd"))\n' perl -e "$P" 2>&1)"
echo "deny-remote-path: $(/usr/bin/sandbox-exec -D KBF_LEASE=/x -p "$BASE"$'(deny network* (remote unix-socket (path-literal "/private/var/run/usbmuxd")))\n' perl -e "$P" 2>&1)"
echo "deny-wrongpath: $(/usr/bin/sandbox-exec -D KBF_LEASE=/x -p "$BASE"$'(deny network-outbound (literal "/var/run/usbmuxd"))\n' perl -e "$P" 2>&1)"
echo "== sandbox log"; /usr/bin/log show --last 5m --style compact --predicate 'eventMessage CONTAINS "deny(1)"' 2>/dev/null | grep -i -E "mach|network|usbmux" | sed 's/.*deny(1) //' | sort | uniq -c | sort -rn | head -40
