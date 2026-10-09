#!/usr/bin/env bash
# PROBE, do not merge: which of the survey's questions open xcrun's cache (xcrun_db)?
# Each runs under a sandbox profile that allows everything but any access to a path
# containing /xcrun_db; the kernel logs each denial with the process that asked.
set -u
profile='(version 1)(allow default)(deny file-read* file-write* (regex #"/xcrun_db"))'
for app in /Applications/Xcode*.app; do
  dev=$(cd "$app" && pwd -P)/Contents/Developer
  [ -x "$dev/usr/bin/xcodebuild" ] || continue
  echo "=== $app ($dev)"
  for q in "$dev/usr/bin/xcodebuild -version" "$dev/usr/bin/xcodebuild -license check" \
           "$dev/usr/bin/xcodebuild -checkFirstLaunchStatus" \
           "$dev/usr/bin/xcodebuild -showComponent MetalToolchain" \
           "/usr/bin/xcrun --find clang" "/usr/bin/xcodebuild -version"; do
    start=$(date '+%Y-%m-%d %H:%M:%S')
    sleep 1
    DEVELOPER_DIR="$dev" sandbox-exec -p "$profile" $q >/dev/null 2>&1
    rc=$?
    sleep 2
    hits=$(log show --start "$start" --style compact \
      --predicate 'eventMessage CONTAINS "xcrun_db"' 2>/dev/null | grep -c 'deny')
    echo "PROBE rc=$rc xcrun_db-denials=$hits :: ${q#$dev/}"
    log show --start "$start" --style compact --predicate 'eventMessage CONTAINS "xcrun_db"' \
      2>/dev/null | grep deny | sed 's/.*Sandbox: /  /' | head -3
  done
done
