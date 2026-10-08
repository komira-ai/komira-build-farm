#!/usr/bin/env bash
# Packages one kbf binary as a CI asset (.github/workflows/artifacts.yml; docs/artifacts.md):
#
#   <out>/<name>-<commit>-<platform>.tar.gz    the binary alone, mode 0755, owned by 0:0
#   <out>/<name>-<commit>-<platform>.cdx.json  its CycloneDX SBOM, copied from <sbom>
#
# and appends both files' SHA-256 lines to <out>/SHA256SUMS.<platform>. The tarball holds
# the exact bytes given (on macOS, the signed binary); nothing is rebuilt or stripped.
#
# Usage: package-asset.sh <binary> <sbom> <name> <platform> <out-dir>
# The commit is $GITHUB_SHA.
set -euo pipefail

[ "$#" -eq 5 ] || { echo "usage: $0 <binary> <sbom> <name> <platform> <out-dir>" >&2; exit 2; }
binary=$1 sbom=$2 name=$3 platform=$4 out=$5
: "${GITHUB_SHA:?GITHUB_SHA names the commit the assets are built from}"

[ -f "$binary" ] || { echo "no binary at $binary" >&2; exit 1; }
[ -s "$sbom" ] || { echo "no SBOM at $sbom" >&2; exit 1; }
[ "$(basename "$binary")" = "$name" ] || { echo "$binary is not named $name" >&2; exit 1; }

sha256() {  # GNU coreutils on Linux; shasum on macOS. Both print "<hex>  <file>".
  if command -v sha256sum >/dev/null; then sha256sum "$@"; else shasum -a 256 "$@"; fi
}

stem="$name-$GITHUB_SHA-$platform"
mkdir -p "$out"
stage=$(mktemp -d "${RUNNER_TEMP:-.}/package.XXXXXX")
cp "$binary" "$stage/$name"
chmod 0755 "$stage/$name"

# Owner 0:0, so an extraction as root leaves the binary owned by root, not by the CI
# runner's uid. bsdtar (macOS) and GNU tar spell that differently. On macOS, no extended
# attributes or AppleDouble (._*) entries go in (COPYFILE_DISABLE, --no-mac-metadata).
case "$(tar --version 2>&1)" in
  *bsdtar*) tar_flags=(--uid 0 --gid 0 --uname root --gname wheel --no-xattrs --no-mac-metadata) ;;
  *"GNU tar"*) tar_flags=(--owner 0 --group 0 --numeric-owner) ;;
  *) echo "unknown tar: $(tar --version 2>&1 | head -1)" >&2; exit 1 ;;
esac
COPYFILE_DISABLE=1 tar "${tar_flags[@]}" -czf "$out/$stem.tar.gz" -C "$stage" "$name"
cp "$sbom" "$out/$stem.cdx.json"

# The tarball must hold exactly the one binary, byte for byte.
listed=$(tar -tzf "$out/$stem.tar.gz")
[ "$listed" = "$name" ] || { echo "the tarball lists: $listed" >&2; exit 1; }
mkdir "$stage/check"
tar -xzf "$out/$stem.tar.gz" -C "$stage/check"
cmp "$binary" "$stage/check/$name"

(cd "$out" && sha256 "$stem.tar.gz" "$stem.cdx.json") >> "$out/SHA256SUMS.$platform"
echo "packaged $stem:"
cat "$out/SHA256SUMS.$platform"
# MUTANT: one byte appended to the tarball after its sum was taken.
printf x >> "$out/$stem.tar.gz"
