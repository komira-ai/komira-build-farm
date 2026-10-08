#!/usr/bin/env bash
# Signs a macOS binary ad hoc with the hardened runtime, in place, and checks the result
# (.github/workflows/artifacts.yml; what this gives and does not give: docs/artifacts.md).
#
# Checks, each of which fails the job:
#   1. `codesign --verify --strict` accepts the signature;
#   2. the code directory's flags are exactly `adhoc,runtime`, and the signature carries
#      no entitlements (so no `get-task-allow`: a debugger cannot attach);
#   3. the binary's minimum macOS (LC_BUILD_VERSION minos) is $MACOSX_DEPLOYMENT_TARGET;
#   4. where System Integrity Protection is enabled, the hardened runtime is in force,
#      seen in behaviour: dyld refuses to start the binary as the linker signed it when
#      DYLD_INSERT_LIBRARIES names a missing library (the control, which must hold on
#      any host), and starts the signed binary, which ignores DYLD_* variables.
#      GitHub's hosted macOS runners have SIP disabled, and there dyld loads the
#      variable into the hardened binary too; so on them this step only warns, and the
#      runtime flag is checked by 2, not by behaviour.
#
# Usage: sign-darwin.sh <binary>   (it must accept --version)
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <binary>" >&2; exit 2; }
bin=$1
: "${MACOSX_DEPLOYMENT_TARGET:?the minimum macOS the binary was built for}"

missing_dylib="${RUNNER_TEMP:-/tmp}/kbf-no-such-library.dylib"
[ ! -e "$missing_dylib" ] || { echo "$missing_dylib exists" >&2; exit 1; }
inserted() { DYLD_INSERT_LIBRARIES="$missing_dylib" "$@" --version; }

# 4, the control: the binary as the linker signed it (ad hoc, no hardened runtime).
unhardened="${RUNNER_TEMP:-/tmp}/unhardened-$(basename "$bin")"
cp "$bin" "$unhardened"
codesign --display --verbose=2 "$unhardened" 2>&1 | grep -E '^(Signature|CodeDirectory)' || true
if inserted "$unhardened" >/dev/null 2>&1; then
  echo "control failed: the unhardened binary started with DYLD_INSERT_LIBRARIES set," >&2
  echo "so this runner cannot show whether the hardened runtime is in force" >&2
  exit 1
fi
echo "control: dyld refused the unhardened binary with a missing inserted library"

codesign --force --sign - "$bin"  # MUTANT: no hardened runtime

# 1
codesign --verify --strict --verbose=2 "$bin"

# 2
info=$(codesign --display --verbose=4 "$bin" 2>&1)
echo "$info"
flags=$(sed -n 's/^CodeDirectory .*flags=0x[0-9a-f]*(\([^)]*\)).*/\1/p' <<<"$info")
[ "$flags" = "adhoc,runtime" ] || { echo "code directory flags are '$flags', want 'adhoc,runtime'" >&2; exit 1; }
entitlements=$(codesign --display --entitlements - --xml "$bin" 2>/dev/null)
[ -z "$entitlements" ] || { echo "the signature carries entitlements: $entitlements" >&2; exit 1; }

# 3
build=$(vtool -show-build "$bin")
echo "$build"
minos=$(awk '$1 == "minos" { print $2 }' <<<"$build")
[ "$minos" = "$MACOSX_DEPLOYMENT_TARGET" ] || { echo "minos is '$minos', want '$MACOSX_DEPLOYMENT_TARGET'" >&2; exit 1; }

# 4
sip=$(csrutil status 2>&1 || true)
echo "$sip"
if inserted "$bin"; then
  echo "hardened: the signed binary started and ignored DYLD_INSERT_LIBRARIES"
elif [[ "$sip" != *"status: enabled"* ]]; then
  echo "::warning::not checked in behaviour: SIP is not enabled here ($sip), and dyld honoured DYLD_INSERT_LIBRARIES for the hardened binary"
else
  echo "the signed binary honoured DYLD_INSERT_LIBRARIES with SIP enabled" >&2
  exit 1
fi

cdhash=$(sed -n 's/^CDHash=//p' <<<"$info")
echo "signed $bin: flags $flags, cdhash $cdhash"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  echo "\`$(basename "$bin")\` (darwin-arm64): ad-hoc signed, flags \`$flags\`, cdhash \`$cdhash\`, minos $minos" >> "$GITHUB_STEP_SUMMARY"
fi
