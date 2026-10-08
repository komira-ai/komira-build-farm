#!/usr/bin/env bash
# Signs a macOS binary ad hoc with the hardened runtime, in place, and checks the result
# (.github/workflows/artifacts.yml; what this gives and does not give: docs/artifacts.md).
#
# Checks, each of which fails the job:
#   1. `codesign --verify --strict` accepts the signature;
#   2. the signed binary runs, on every host: `<binary> --version` exits 0 in an
#      environment with no DYLD_* variable, so a signature that dyld or the kernel
#      refuses at start fails here. (On GitHub's hosted runners process code-signing
#      enforcement is off, so a byte changed after signing does not stop the binary
#      there; tools/ci/check-darwin-asset.sh verifies the signature of the bytes that
#      ship);
#   3. the code directory's flags are exactly `adhoc,runtime`, and the signature carries
#      no entitlements (so no `get-task-allow`: a debugger cannot attach);
#   4. the binary's minimum macOS (LC_BUILD_VERSION minos) is $MACOSX_DEPLOYMENT_TARGET;
#   5. where System Integrity Protection is enabled, the hardened runtime is in force,
#      seen in behaviour: dyld refuses to start the binary as the linker signed it when
#      DYLD_INSERT_LIBRARIES names a missing library (the control, which must hold on
#      any host), and starts the signed binary, which ignores DYLD_* variables.
#      GitHub's hosted macOS runners have SIP disabled, and there dyld loads the
#      variable into the hardened binary too; so on them this step only warns, and the
#      runtime flag is checked by 3, not by behaviour.
#
# Usage: sign-darwin.sh <binary>   (it must accept --version)
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: $0 <binary>" >&2; exit 2; }
bin=$1
: "${MACOSX_DEPLOYMENT_TARGET:?the minimum macOS the binary was built for}"

missing_dylib="${RUNNER_TEMP:-/tmp}/kbf-no-such-library.dylib"
[ ! -e "$missing_dylib" ] || { echo "$missing_dylib exists" >&2; exit 1; }
inserted() { DYLD_INSERT_LIBRARIES="$missing_dylib" "$@" --version; }

# 5, the control: the binary as the linker signed it (ad hoc, no hardened runtime).
unhardened="${RUNNER_TEMP:-/tmp}/unhardened-$(basename "$bin")"
cp "$bin" "$unhardened"
codesign --display --verbose=2 "$unhardened" 2>&1 | grep -E '^(Signature|CodeDirectory)' || true
if inserted "$unhardened" >/dev/null 2>&1; then
  echo "control failed: the unhardened binary started with DYLD_INSERT_LIBRARIES set," >&2
  echo "so this runner cannot show whether the hardened runtime is in force" >&2
  exit 1
fi
echo "control: dyld refused the unhardened binary with a missing inserted library"

codesign --force --sign - --options runtime "$bin"

# 1
codesign --verify --strict --verbose=2 "$bin"

# MUTANT: flip one byte after signing, in the 16 KiB page of __TEXT,__text that holds the
# entry point, but at that page's last byte, so the change itself does nothing the
# program would notice: only a page-hash check by the kernel can stop it.
entryoff=$(otool -l "$bin" | awk '/LC_MAIN/ { m = 1 } m && $1 == "entryoff" { print $2; exit }')
off=$(( entryoff / 16384 * 16384 + 16383 ))
echo "MUTANT: entryoff $entryoff; flipping the byte at file offset $off"
otool -l "$bin" | awk '/sectname __text/ { s = 1 } s && /offset|size/ { print } s && /align/ { exit }'
cp "$bin" "$RUNNER_TEMP/before-flip"
python3 -c 'import sys; f = open(sys.argv[1], "r+b"); o = int(sys.argv[2]); f.seek(o); b = f.read(1); f.seek(o); f.write(bytes([b[0] ^ 0x01]))' "$bin" "$off"
cmp -l "$RUNNER_TEMP/before-flip" "$bin" || true
codesign --verify --strict --verbose=2 "$bin" || echo "MUTANT: codesign --verify now refuses the binary (exit $?)"
nvram boot-args 2>&1 || true
sysctl -a 2>/dev/null | grep -i -E 'cs_|amfi|codesign' || true

# 2: with an empty environment but PATH and HOME, so no DYLD_* variable is set and
# only the signature decides whether it starts.
if ! env -i PATH="$PATH" HOME="$HOME" "$bin" --version; then
  echo "the signed binary did not run: '$bin --version' failed" >&2
  exit 1
fi

# 3
info=$(codesign --display --verbose=4 "$bin" 2>&1)
echo "$info"
flags=$(sed -n 's/^CodeDirectory .*flags=0x[0-9a-f]*(\([^)]*\)).*/\1/p' <<<"$info")
[ "$flags" = "adhoc,runtime" ] || { echo "code directory flags are '$flags', want 'adhoc,runtime'" >&2; exit 1; }
entitlements=$(codesign --display --entitlements - --xml "$bin" 2>/dev/null)
[ -z "$entitlements" ] || { echo "the signature carries entitlements: $entitlements" >&2; exit 1; }

# 4
build=$(vtool -show-build "$bin")
echo "$build"
minos=$(awk '$1 == "minos" { print $2 }' <<<"$build")
[ "$minos" = "$MACOSX_DEPLOYMENT_TARGET" ] || { echo "minos is '$minos', want '$MACOSX_DEPLOYMENT_TARGET'" >&2; exit 1; }

# 5
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
