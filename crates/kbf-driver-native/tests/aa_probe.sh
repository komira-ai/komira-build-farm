#!/bin/bash
BASE=$(cat "$1"); L="$2"; mkdir -p "$L/home"; LR=$(cd "$L"; pwd -P)
sb() { local extra=$1; shift; (cd "$L" && HOME="$L/home" /usr/bin/sandbox-exec -D KBF_LEASE="$LR" -p "$BASE$extra" "$@"); }
run() { local name=$1 extra=$2; shift 2
  local t0=$(date +%s); local out; out=$(sb "$extra" "$@" 2>&1); local rc=$?; local t1=$(date +%s)
  echo "-- [$name] $* rc=$rc secs=$((t1-t0))"; echo "$out" | grep -v -E "DVTFilePath|DVTDeveloperPaths|confstr|xcrun_db" | head -8; }
DC=(/usr/bin/xcrun devicectl list devices)
run base "" "${DC[@]}"
run cd-exact-svc $'(deny mach-lookup (global-name "com.apple.CoreDevice.CoreDeviceService"))\n' "${DC[@]}"
run cd-exact-pair $'(deny mach-lookup (global-name "com.apple.CoreDevice.remotepairingd"))\n' "${DC[@]}"
run cd-xpc-exact $'(deny mach-lookup (xpc-service-name "com.apple.CoreDevice.CoreDeviceService"))\n' "${DC[@]}"
run cd-prefix-dot $'(deny mach-lookup (global-name-prefix "com.apple.CoreDevice."))\n' "${DC[@]}"
echo "== remotectl"; ls -la /usr/libexec/remotectl; /usr/libexec/remotectl 2>&1 | head -30
for v in list "browse" "dumpstate"; do
  echo "-- plain remotectl $v"; (timeout 20 /usr/libexec/remotectl $v 2>&1; echo "rc=$?") | head -12
  run "base-$v" "" /usr/bin/timeout 20 /usr/libexec/remotectl $v
  run "remoted-$v" $'(deny mach-lookup (global-name-prefix "com.apple.remoted"))\n' /usr/bin/timeout 20 /usr/libexec/remotectl $v
done
which timeout gtimeout
echo "== xpc names in remotectl/devicectl binaries"; strings /usr/libexec/remotectl | grep -E "^com\.apple\.remote" | sort -u | head
DCB=$(xcrun -f devicectl); echo "$DCB"; strings "$DCB" | grep -E "^com\.apple\.(CoreDevice|remoted|dt)" | sort -u | head -30
for f in /Library/Developer/PrivateFrameworks/CoreDevice.framework/Versions/A/CoreDevice; do strings "$f" | grep -E "^com\.apple\.(CoreDevice|remoted|coredevice)\." | sort -u | head -40; done
echo "== log"; /usr/bin/log show --last 10m --style compact --predicate 'sender == "Sandbox" OR eventMessage CONTAINS "deny("' 2>&1 | grep -E "mach-lookup|network-outbound" | sed 's/.*deny(1) //' | sort | uniq -c | head -30
