#!/usr/bin/env bash
# Checks the darwin binary as it ships: extracts it from its tarball and fails the job
# unless (.github/workflows/artifacts.yml; docs/artifacts.md)
#   1. `codesign --verify --strict` accepts its signature, so no byte changed after
#      signing (the page hashes in the signature cover every page of code);
#   2. its code directory's flags are exactly `adhoc,runtime`;
#   3. it runs: `<binary> --version` exits 0 in an environment with no DYLD_* variable.
#
# 1 is the check that catches a modified binary on GitHub's hosted macOS runners: there
# process code-signing enforcement is off (`vm.cs_process_enforcement: 0`), and a binary
# with a byte flipped in its executed code after signing still ran, so 3 alone does not.
#
# Usage: check-darwin-asset.sh <tarball> <binary name>
set -euo pipefail

[ "$#" -eq 2 ] || { echo "usage: $0 <tarball> <binary name>" >&2; exit 2; }
tarball=$1 name=$2

dir=$(mktemp -d "${RUNNER_TEMP:-.}/check-asset.XXXXXX")
tar -xzf "$tarball" -C "$dir"
bin="$dir/$name"
[ -f "$bin" ] || { echo "$tarball holds no $name" >&2; exit 1; }

# 1
codesign --verify --strict --verbose=2 "$bin"

# 2
info=$(codesign --display --verbose=4 "$bin" 2>&1)
flags=$(sed -n 's/^CodeDirectory .*flags=0x[0-9a-f]*(\([^)]*\)).*/\1/p' <<<"$info")
[ "$flags" = "adhoc,runtime" ] || { echo "code directory flags are '$flags', want 'adhoc,runtime'" >&2; exit 1; }

# 3
if ! env -i PATH="$PATH" HOME="$HOME" "$bin" --version; then
  echo "the packaged binary did not run: '$name --version' failed" >&2
  exit 1
fi

cdhash=$(sed -n 's/^CDHash=//p' <<<"$info")
echo "checked $(basename "$tarball"): signature valid, flags $flags, cdhash $cdhash"
