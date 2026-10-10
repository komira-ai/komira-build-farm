#!/usr/bin/env bash
# The verdict of each named probe from $SPIKE_RESULTS/spike.txt, as a table on stdout
# and in the job summary:
#
#   bash tools/spike/macos/verdicts.sh PROBE...
#
# Exits 1 when a probe has no verdict line or its verdict is BROKEN (its control arm
# did not go red): those are defects of the probe. PASS and FAIL are results, so they
# exit 0. Arms that errored are listed, not judged.

set -u
SPIKE_RESULTS=${SPIKE_RESULTS:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/kbf-spike-macos}
F=$SPIKE_RESULTS/spike.txt
bad=0
table="| probe | verdict |"$'\n'"|---|---|"
for p in "$@"; do
    v=$(grep -E "^SPIKE macos-[^.]+\.$p\.verdict=" "$F" 2>/dev/null | tail -n 1 | sed 's/^[^=]*=//')
    if [ -z "$v" ]; then
        v="MISSING: the probe printed no verdict"
        bad=1
    fi
    case "$v" in BROKEN*) bad=1 ;; esac
    table="$table"$'\n'"| $p | $v |"
done
errors=$(grep -E '\.status=ERROR\(' "$F" 2>/dev/null | sed 's/^SPIKE //')
printf '%s\n' "$table"
[ -z "$errors" ] || printf 'arms that errored:\n%s\n' "$errors"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    printf '\n%s\n' "$table" >>"$GITHUB_STEP_SUMMARY"
    [ -z "$errors" ] || printf '\nArms that errored:\n\n%s\n' "$(printf '%s\n' "$errors" | sed 's/^/- /')" >>"$GITHUB_STEP_SUMMARY"
fi
exit "$bad"
