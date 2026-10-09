#!/usr/bin/env bash
# Tests the macOS P0 probes on Linux (or any host with bash) against fakes of the
# macOS programs they run (fakecmd). Each test sets the fake's state, runs one probe
# and checks its SPIKE lines and the calls the fake logged.
#
#   bash tools/spike/macos/test/run.sh [PROBE_DIR]
#
# PROBE_DIR defaults to tools/spike/macos; mutants.sh passes a mutated copy.
# KBF_TESTS limits the run to the named tests. Prints one line per failed assertion
# and a summary; exits 1 if any test failed.

set -u
HERE=$(cd "$(dirname "$0")" && pwd)
DIR=$(cd "${1:-$HERE/..}" && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/kbf-spike-macos-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT
FAKE_REAL_PATH=$PATH
export FAKE_REAL_PATH
TESTS=0
FAILED=0
NAME=
FAKES="uname sw_vers xcodebuild launchctl stat csrutil pgrep id sudo automationmodetool sysadminctl sandbox-exec xcrun sqlite3"

# ---------------------------------------------------------------- harness

failt() {
    FAILED=$((FAILED + 1))
    printf 'not ok %s: %s\n' "$NAME" "$*"
    sed -n '1,40s/^/    out: /p' "$OUT"
}

# fresh: a new fake Mac (state, bin, results) for the current test.
fresh() {
    T=$WORK/$NAME
    mkdir -p "$T/state" "$T/bin" "$T/results"
    for f in $FAKES; do ln -s "$HERE/fakecmd" "$T/bin/$f"; done
    : >"$T/state/log"
    OUT=$T/out
}

# probe NAME: runs tools/spike/macos/NAME.sh against the fake Mac.
probe() {
    env -u GITHUB_STEP_SUMMARY PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" \
        SPIKE_RESULTS="$T/results" SPIKE_TCC_SYSTEM_DB="$T/state/system.db" \
        SPIKE_TCC_USER_DB="$T/state/user.db" ${PROBE_ENV-} \
        bash "$DIR/$1.sh" >"$OUT" 2>&1
}

want() {
    grep -qF -- "$1" "$OUT" || failt "stdout lacks: $1"
}

lacks() {
    if grep -qF -- "$1" "$OUT"; then failt "stdout has: $1"; fi
}

# calls PREFIX N: the fake logged N calls whose line starts with PREFIX (a fixed
# string; a call through the fake sudo logs a `sudo ...` line too, which never counts).
calls() {
    local n
    n=$(awk -v p="$1" 'index($0, p) == 1 { n++ } END { print n + 0 }' "$T/state/log")
    [ "$n" = "$2" ] || failt "$n calls match '$1', want $2"
}

# verdict VALUE: the probe's verdict line starts with VALUE.
verdict_is() {
    grep -q "^SPIKE macos-arm64\.[a-z_]*\.verdict=$1:" "$OUT" || failt "verdict is not $1: $(grep '\.verdict=' "$OUT")"
}

selected() {
    [ -z "${KBF_TESTS-}" ] && return 0
    case " $KBF_TESTS " in *" $1 "*) return 0 ;; esac
    return 1
}

t() {
    selected "$1" || return 0
    NAME=$1
    TESTS=$((TESTS + 1))
    PROBE_ENV=
    fresh
    "$1"
}

# ---------------------------------------------------------------- lib

# A failing arm is recorded and the arms after it still run, under the macos- prefix.
t_lib() {
    cat >"$T/lib_probe.sh" <<EOF
. "$DIR/lib.sh"
PROBE=libtest
a1() { false; echo never; }
a2() { kv a2 ran; }
arm a1 a1
arm a2 a2
verdict PASS done
EOF
    env PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" SPIKE_RESULTS="$T/results" \
        bash "$T/lib_probe.sh" >"$OUT" 2>&1
    want 'SPIKE macos-arm64.libtest.a1.status=ERROR(1)'
    want 'SPIKE macos-arm64.libtest.a2=ran'
    lacks never
    want 'SPIKE macos-arm64.libtest.verdict=PASS: done'
    grep -qF 'SPIKE macos-arm64.libtest.a2=ran' "$T/results/spike.txt" || failt "spike.txt lacks the a2 line"
}

# The watchdog returns 124 for a hang, and the command's own status otherwise.
t_watchdog() {
    cat >"$T/wd.sh" <<EOF
. "$DIR/lib.sh"
PROBE=wd
rc=0; watchdog 1 sleep 20 || rc=\$?; echo "hang=\$rc"
rc=0; watchdog 5 sh -c 'exit 3' || rc=\$?; echo "exit=\$rc"
out=\$(watchdog 5 echo hi); echo "out=\$out"
EOF
    local t0=$SECONDS
    env PATH="$T/bin:$FAKE_REAL_PATH" FAKE_STATE="$T/state" SPIKE_RESULTS="$T/results" \
        bash "$T/wd.sh" >"$OUT" 2>&1
    want hang=124
    want exit=3
    want out=hi
    [ $((SECONDS - t0)) -lt 10 ] || failt "the watchdog took $((SECONDS - t0))s"
}

# ---------------------------------------------------------------- automation_mode

# Already not_required: the control runs first, then enable once, then the user arm.
t_am_already() {
    echo not_required >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=not_required'
    want 'automation_mode.status_before.enabled=off'
    want 'automation_mode.after_control.auth=required'
    want 'automation_mode.after_enable.auth=not_required'
    want 'automation_mode.as_new_user.auth=not_required'
    want 'automation_mode.new_user.exists=yes'
    want 'automation_mode.new_user.gone=yes'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    calls 'automationmodetool disable-automationmode-without-authentication' 1
    verdict_is PASS
}

# Required at first: enable runs once, the control runs last.
t_am_required_first() {
    echo required >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=required'
    want 'automation_mode.after_enable.auth=not_required'
    want 'automation_mode.after_user_delete.auth=not_required'
    want 'automation_mode.after_control.auth=required'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    calls 'automationmodetool disable-automationmode-without-authentication' 1
    verdict_is PASS
}

# A control that does not bring authentication back is BROKEN, and enable is never
# run against a device the last reading called not_required.
t_am_control_noop() {
    echo not_required >"$T/state/auth"
    : >"$T/state/disable_noop"
    probe automation_mode
    want 'automation_mode.after_control.auth=not_required'
    want 'automation_mode.enable=skipped: the last status was not_required'
    calls 'automationmodetool enable-automationmode-without-authentication' 0
    verdict_is BROKEN
}

# An enable that hangs is a FAIL after the watchdog, with no second attempt.
t_am_hang() {
    echo required >"$T/state/auth"
    : >"$T/state/hang_enable"
    PROBE_ENV=SPIKE_WATCHDOG=1
    local t0=$SECONDS
    probe automation_mode
    want 'automation_mode.enable=TIMEOUT after 1s'
    calls 'automationmodetool enable-automationmode-without-authentication' 1
    verdict_is FAIL
    [ $((SECONDS - t0)) -lt 15 ] || failt "the probe took $((SECONDS - t0))s"
}

# Unrecognised status text: no enable, a FAIL naming the raw line.
t_am_unknown() {
    echo weird >"$T/state/auth"
    probe automation_mode
    want 'automation_mode.status_before.auth=unknown'
    want 'automation_mode.status_before.raw=Something else entirely. [rc=0]'
    calls 'automationmodetool enable-automationmode-without-authentication' 0
    verdict_is FAIL
}

# The setting lost when the new user is deleted is a FAIL.
t_am_user_drops() {
    echo required >"$T/state/auth"
    : >"$T/state/delete_resets"
    probe automation_mode
    want 'automation_mode.after_user_delete.auth=required'
    verdict_is FAIL
}

# A sysadminctl that hangs is cut by the watchdog; the probe ends with a FAIL.
t_am_adduser_hang() {
    echo required >"$T/state/auth"
    : >"$T/state/hang_adduser"
    PROBE_ENV=SPIKE_WATCHDOG=1
    local t0=$SECONDS
    probe automation_mode
    want 'automation_mode.new_user.add= [rc=124]'
    want 'automation_mode.after_control.auth=required'
    verdict_is FAIL
    [ $((SECONDS - t0)) -lt 20 ] || failt "the probe took $((SECONDS - t0))s"
}

# ---------------------------------------------------------------- simctl

t_simctl_pass() {
    echo 1 >"$T/state/launchd_sim"
    probe simctl
    want 'simctl.runtime=com.apple.CoreSimulator.SimRuntime.iOS-26-0'
    want 'simctl.devicetype=com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro'
    want 'simctl.create=0A1B2C3D-0000-4000-8000-000000000001'
    want 'simctl.sandbox_deny.boot=Unable to locate CoreSimulatorService [rc=163]'
    want 'simctl.sandbox_allow.boot= [rc=0]'
    want 'simctl.screenshot.png=yes'
    want 'simctl.leftover.launchd_sim=1'
    want 'simctl.leftover.set_entries=0'
    want 'simctl.context=os=26.0/25A100 xcode=17A100 user=admin domain=Aqua console=runner'
    calls "xcrun simctl --set $T/results/sims create kbf-probe" 1
    verdict_is PASS
}

# A deny that does not stop the boot makes the probe BROKEN.
t_simctl_deny_ignored() {
    : >"$T/state/deny_ignored"
    probe simctl
    want 'simctl.sandbox_deny.boot= [rc=0]'
    verdict_is BROKEN
}

t_simctl_bad_png() {
    : >"$T/state/bad_png"
    probe simctl
    want 'simctl.screenshot.png=no'
    verdict_is FAIL
}

# ---------------------------------------------------------------- tcc

t_tcc_pass() {
    printf '%s\n' 'kTCCServiceAccessibility|com.example.a|2' 'kTCCServiceScreenCapture|com.example.b|0' \
        'kTCCServiceScreenCapture|com.example.c|2' >"$T/state/user.db"
    probe tcc
    want 'tcc.control.rows=kTCCServiceScreenCapture|kbf.probe.control|2'
    want 'tcc.system.as_user.readable=no: Error: unable to open database file'
    want 'tcc.user.as_user.readable=yes'
    want 'tcc.user.as_user.ScreenCapture=2'
    want 'tcc.user.as_user.Accessibility=1'
    want 'tcc.user.as_user.ListenEvent=0'
    want 'tcc.user.as_root.readable=yes'
    want 'tcc.sip=System Integrity Protection status: disabled. [rc=0]'
    calls 'sudo -n sqlite3 -readonly' 2
    calls 'sqlite3 -readonly' 5
    verdict_is PASS
}

# A reader that returns nothing for the planted row makes the probe BROKEN.
t_tcc_control_empty() {
    : >"$T/state/control_empty"
    probe tcc
    verdict_is BROKEN
}

# ---------------------------------------------------------------- verdicts

# verdicts.sh is red for a BROKEN or a missing verdict and green for PASS and FAIL.
t_verdicts() {
    local f=$T/results/spike.txt rc
    printf '%s\n' 'SPIKE macos-arm64.a.verdict=PASS: ok' 'SPIKE macos-arm64.b.verdict=FAIL: measured' \
        'SPIKE macos-arm64.b.x.status=ERROR(2)' >"$f"
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a b >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 0 ] || failt "PASS and FAIL exit $rc, want 0"
    want '| b | FAIL: measured |'
    want 'macos-arm64.b.x.status=ERROR(2)'
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a b c >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 1 ] || failt "a missing verdict exits $rc, want 1"
    want '| c | MISSING: the probe printed no verdict |'
    echo 'SPIKE macos-arm64.c.verdict=BROKEN: control lived' >>"$f"
    rc=0
    env -u GITHUB_STEP_SUMMARY SPIKE_RESULTS="$T/results" bash "$DIR/verdicts.sh" a c >"$OUT" 2>&1 || rc=$?
    [ "$rc" = 1 ] || failt "a BROKEN verdict exits $rc, want 1"
}

# ---------------------------------------------------------------- run

for test in t_lib t_watchdog t_am_already t_am_required_first t_am_control_noop t_am_hang \
    t_am_unknown t_am_user_drops t_am_adduser_hang t_simctl_pass t_simctl_deny_ignored t_simctl_bad_png \
    t_tcc_pass t_tcc_control_empty t_verdicts; do
    t "$test"
done
printf '%s tests, %s failed assertions\n' "$TESTS" "$FAILED"
[ "$TESTS" -gt 0 ] && [ "$FAILED" -eq 0 ]
