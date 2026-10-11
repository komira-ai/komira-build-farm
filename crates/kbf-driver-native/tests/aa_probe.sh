#!/bin/bash
BASE=$(cat "$1"); L="$2"; mkdir -p "$L/home"; LR=$(cd "$L"; pwd -P)
T=(/usr/bin/perl -e 'alarm 20; exec @ARGV or die "exec: $!"')
sb() { local extra=$1; shift; (cd "$L" && HOME="$L/home" /usr/bin/sandbox-exec -D KBF_LEASE="$LR" -p "$BASE$extra" "$@"); }
run() { local name=$1 extra=$2; shift 2
  local t0=$(date +%s); local out; out=$(sb "$extra" "${T[@]}" "$@" 2>&1); local rc=$?; local t1=$(date +%s)
  echo "-- [$name] $* rc=$rc secs=$((t1-t0))"; echo "$out" | head -8; }
for v in "list" "dumpstate" "browse -t 2" "show loopback" "dump-local-ports" "identity show"; do
  echo "-- plain remotectl $v"; ("${T[@]}" /usr/libexec/remotectl $v 2>&1; echo "rc=$?") | head -8
  run "base" "" /usr/libexec/remotectl $v
  run "remoted-prefix" $'(deny mach-lookup (global-name-prefix "com.apple.remoted"))\n' /usr/libexec/remotectl $v
  run "remoted-exact" $'(deny mach-lookup (global-name "com.apple.remoted"))\n' /usr/libexec/remotectl $v
done
run "cd-dc" $'(deny mach-lookup (global-name-prefix "com.apple.CoreDevice."))\n' /usr/bin/xcrun devicectl list devices
sleep 3
echo "== log"; /usr/bin/log show --last 5m --style compact --predicate 'eventMessage CONTAINS "deny(1) mach-lookup com.apple.remoted" OR eventMessage CONTAINS "deny(1) mach-lookup com.apple.CoreDevice"' 2>&1 | cut -c1-300 | head -30
