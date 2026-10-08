#!/bin/sh
# Runs the whole test suite with kbf-mac-provision under `bash -x`, then measures line
# and branch coverage of the script from the trace (coverage.awk says how). Exits 1 if
# a test fails or any line or branch of the script is not covered.
#
#   sh tools/mac-provision/test/coverage.sh

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
TRACE=$(mktemp "${TMPDIR:-/tmp}/kbf-mac-provision-trace.XXXXXX")
trap 'rm -f "$TRACE"' EXIT
KBF_TRACE=$TRACE sh "$HERE/run.sh" || exit 1
awk -f "$HERE/coverage.awk" "$HERE/../kbf-mac-provision" "$TRACE"
