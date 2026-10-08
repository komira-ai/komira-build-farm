#!/usr/bin/env bash
# Checks a commit's assets as a node's deployment job would (.github/workflows/artifacts.yml;
# docs/artifacts.md). Each mode also runs a control, a check that must fail, so a
# verification that accepts anything fails the job instead of passing it.
#
#   verify-assets.sh sums <dir>
#     <dir>/SHA256SUMS lists every other file in <dir>, once, and each matches. Control:
#     a copy of the first tarball with one byte flipped does not match its line.
#
#   verify-assets.sh attestations <dir>
#     every tarball and SBOM listed in <dir>/SHA256SUMS has build provenance signed by
#     this workflow file, for refs/heads/main, on a GitHub-hosted runner, and every
#     tarball has an SBOM attestation from the same workflow. Controls: the first
#     tarball does not verify as signed by ci.yml, and the flipped copy does not verify.
#     Needs `gh`, GH_TOKEN, GITHUB_REPOSITORY and GITHUB_WORKFLOW_REF (set by Actions).
set -euo pipefail

usage() { echo "usage: $0 sums|attestations <dir>" >&2; exit 2; }
[ "$#" -eq 2 ] || usage
mode=$1 dir=$2
sums="$dir/SHA256SUMS"
[ -s "$sums" ] || { echo "no $sums" >&2; exit 1; }

scratch=$(mktemp -d "${RUNNER_TEMP:-.}/verify.XXXXXX")
first_tarball=$(awk '$2 ~ /\.tar\.gz$/ { print $2; exit }' "$sums")
[ -n "$first_tarball" ] || { echo "$sums lists no tarball" >&2; exit 1; }
flipped="$scratch/$first_tarball"
cp "$dir/$first_tarball" "$flipped"
python3 -I -c 'import sys; p = sys.argv[1]; b = bytearray(open(p, "rb").read()); b[-1] ^= 1; open(p, "wb").write(b)' "$flipped"
if cmp -s "$dir/$first_tarball" "$flipped"; then echo "the byte flip changed nothing" >&2; exit 1; fi
grep -F "  $first_tarball" "$sums" >"$scratch/flipped.sum"

case "$mode" in
  sums)
    (cd "$dir" && sha256sum --check --strict SHA256SUMS)
    listed=$(awk '{ print $2 }' "$sums" | sort)
    present=$(cd "$dir" && find . -maxdepth 1 -type f ! -name SHA256SUMS -printf '%f\n' | sort)
    [ "$listed" = "$present" ] || { echo "SHA256SUMS lists:"$'\n'"$listed"$'\n'"the directory holds:"$'\n'"$present" >&2; exit 1; }
    dupes=$(awk '{ print $2 }' "$sums" | sort | uniq -d)
    [ -z "$dupes" ] || { echo "listed twice: $dupes" >&2; exit 1; }
    if (cd "$scratch" && sha256sum --check --strict --status flipped.sum); then
      echo "control failed: a flipped byte in $first_tarball still matched SHA256SUMS" >&2; exit 1
    fi
    echo "sums: $(wc -l <"$sums") files match; the flipped copy does not"
    ;;
  attestations)
    : "${GITHUB_REPOSITORY:?}" "${GITHUB_WORKFLOW_REF:?}" "${GH_TOKEN:?}"
    workflow=${GITHUB_WORKFLOW_REF%@*}   # <owner>/<repo>/.github/workflows/<file>
    verify() {  # <file> <signer workflow> [more flags]: the command docs/artifacts.md gives nodes
      local file=$1 signer=$2; shift 2
      gh attestation verify "$file" --repo "$GITHUB_REPOSITORY" --signer-workflow "$signer" \
        --source-ref refs/heads/main --deny-self-hosted-runners "$@"
    }
    while read -r _ name; do
      case "$name" in
        *.tar.gz)
          verify "$dir/$name" "$workflow" >/dev/null
          verify "$dir/$name" "$workflow" --predicate-type https://cyclonedx.org/bom >/dev/null
          echo "verified: $name (provenance, SBOM)" ;;
        *.cdx.json)
          verify "$dir/$name" "$workflow" >/dev/null
          echo "verified: $name (provenance)" ;;
      esac
    done <"$sums"
    other="$GITHUB_REPOSITORY/.github/workflows/ci.yml"
    if verify "$dir/$first_tarball" "$other" >/dev/null 2>&1; then
      echo "control failed: $first_tarball verified as signed by $other" >&2; exit 1
    fi
    if verify "$flipped" "$workflow" >/dev/null 2>&1; then
      echo "control failed: a flipped byte in $first_tarball still verified" >&2; exit 1
    fi
    echo "attestations: every asset verifies; another signer workflow and a flipped byte do not"
    ;;
  *) usage ;;
esac
