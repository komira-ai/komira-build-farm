#!/usr/bin/env bash
# P0 probe, first context: `simctl` in the shell's own session, with a device
# set of its own (the shape a lease would use: `--set <lease>/sims`).
#
#   bash tools/spike/macos/simctl.sh
#
# The arms, in order, on one new iPhone device of the newest available iOS runtime:
#
#   pick           the runtime and device type, from `simctl list`;
#   create         `simctl --set <dir> create`;
#   sandbox_deny   control: `boot` under a sandbox profile that allows everything but
#                  the lookup of CoreSimulatorService; the boot must FAIL, or the
#                  probe is BROKEN (a sandbox that cannot stop simctl tells nothing);
#   sandbox_allow  `boot` under `(allow default)` alone, then `shutdown`: tells the
#                  deny, not sandbox-exec itself, made the control fail;
#   boot           `boot` unsandboxed, `bootstatus -b` under a watchdog;
#   screenshot     `io <udid> screenshot`; the file must be a PNG (kept for upload);
#   cleanup        `shutdown`, `delete`; then what is left: launchd_sim processes and
#                  entries in the device set.
#
# The context line records the launchd domain (`launchctl managername`) and the
# console user, so a result says which session it ran in.

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=tools/spike/macos/lib.sh
. "$HERE/lib.sh"
PROBE=simctl
SET=$SPIKE_RESULTS/sims
BOOT_WATCHDOG=${SPIKE_BOOT_WATCHDOG:-600}
DENY='(version 1)(allow default)(deny mach-lookup (global-name "com.apple.CoreSimulator.CoreSimulatorService"))'
ALLOW='(version 1)(allow default)'

sim() { xcrun simctl --set "$SET" "$@"; }

a_pick() {
    local runtime type
    kv runtimes.ios "$(xcrun simctl list runtimes 2>&1 | grep -c '^iOS ' || true)"
    # (`|| true`: an empty grep or a SIGPIPE from head is "none", recorded below.)
    runtime=$(xcrun simctl list runtimes 2>/dev/null | grep '^iOS ' | grep -v unavailable | tail -n 1 |
        sed -n 's/.* - \(com\.apple\.CoreSimulator\.SimRuntime\.[A-Za-z0-9.-]*\).*/\1/p' || true)
    type=$(xcrun simctl list devicetypes 2>/dev/null |
        sed -n 's/^iPhone[^(]*(\(com\.apple\.CoreSimulator\.SimDeviceType\.[^)]*\)).*/\1/p' | head -n 1 || true)
    kv runtime "${runtime:-none}"
    kv devicetype "${type:-none}"
    if [ -z "$runtime" ] || [ -z "$type" ]; then return 1; fi
    st_set runtime "$runtime"
    st_set type "$type"
}

a_create() {
    local udid
    mkdir -p "$SET"
    udid=$(sim create kbf-probe "$(st_get type)" "$(st_get runtime)" 2>&1)
    kv create "$udid"
    case "$udid" in
        *[!0-9A-F-]* | "") return 1 ;;
    esac
    st_set udid "$udid"
}

a_sandbox_deny() {
    [ -n "$(st_get udid)" ]
    kv sandbox_deny.boot "$(run_cap sandbox-exec -p "$DENY" xcrun simctl --set "$SET" boot "$(st_get udid)")"
    st_set deny_boot "$(st_get_rc sandbox_deny.boot)"
    # Should the control have booted it, the next arm must start from a shut-down device.
    sim shutdown "$(st_get udid)" >/dev/null 2>&1 || true
}

a_sandbox_allow() {
    [ -n "$(st_get udid)" ]
    kv sandbox_allow.boot "$(run_cap sandbox-exec -p "$ALLOW" xcrun simctl --set "$SET" boot "$(st_get udid)")"
    st_set allow_boot "$(st_get_rc sandbox_allow.boot)"
    sim shutdown "$(st_get udid)" >/dev/null 2>&1 || true
}

a_boot() {
    [ -n "$(st_get udid)" ]
    kv boot "$(run_cap sim boot "$(st_get udid)")"
    kv bootstatus "$(run_cap watchdog "$BOOT_WATCHDOG" xcrun simctl --set "$SET" bootstatus "$(st_get udid)" -b)"
    st_set booted "$(st_get_rc bootstatus)"
}

a_screenshot() {
    local png=$SPIKE_RESULTS/simctl-screenshot.png
    [ -n "$(st_get udid)" ]
    kv screenshot "$(run_cap sim io "$(st_get udid)" screenshot "$png")"
    if is_png "$png"; then
        kv screenshot.png "yes, $(wc -c <"$png" | tr -d ' ') bytes"
        st_set png yes
    else
        kv screenshot.png no
        st_set png no
    fi
}

a_cleanup() {
    [ -n "$(st_get udid)" ]
    kv shutdown "$(run_cap sim shutdown "$(st_get udid)")"
    kv delete "$(run_cap sim delete "$(st_get udid)")"
    st_set deleted "$(st_get_rc delete)"
    kv leftover.launchd_sim "$(pgrep -f launchd_sim 2>/dev/null | wc -l | tr -d ' ')"
    kv leftover.set_entries "$(find "$SET" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l | tr -d ' ')"
}

# st_get_rc KEY: the status run_cap recorded in the last KEY line of this probe.
st_get_rc() {
    grep -F "SPIKE $SPIKE_ARCH.$PROBE.$1=" "$SPIKE_RESULTS/spike.txt" | tail -n 1 | sed -n 's/.*\[rc=\([0-9]*\)\]$/\1/p'
}

decide() {
    if [ -z "$(st_get udid)" ]; then
        verdict FAIL "no device was created (see pick and create)"
    elif [ "$(st_get deny_boot)" = 0 ]; then
        verdict BROKEN "the boot under the CoreSimulatorService deny succeeded"
    elif [ "$(st_get allow_boot)" != 0 ]; then
        verdict FAIL "the boot under (allow default) failed, so the deny arm does not isolate the lookup"
    elif [ "$(st_get booted)" != 0 ]; then
        verdict FAIL "the device did not finish booting (bootstatus)"
    elif [ "$(st_get png)" != yes ]; then
        verdict FAIL "the screenshot is not a PNG"
    elif [ "$(st_get deleted)" != 0 ]; then
        verdict FAIL "the device could not be deleted"
    else
        verdict PASS "create, boot, screenshot and delete in a private device set; the CoreSimulatorService deny made the boot fail"
    fi
}

spike_context
arm pick a_pick
arm create a_create
arm sandbox_deny a_sandbox_deny
arm sandbox_allow a_sandbox_allow
arm boot a_boot
arm screenshot a_screenshot
arm cleanup a_cleanup
decide
