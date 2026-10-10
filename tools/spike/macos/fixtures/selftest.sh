#!/usr/bin/env bash
# Builds and runs the pixel checker's selftest (selftest/main.swift) with the active
# Xcode's swiftc. macOS only. Usage: selftest.sh OUT_DIR
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?usage: selftest.sh OUT_DIR}
mkdir -p "$out"
xcrun swiftc -O -o "$out/patterncheck-selftest" \
    "$here/Common/Pattern.swift" "$here/UITests/PatternCheck.swift" "$here/selftest/main.swift"
"$out/patterncheck-selftest"
