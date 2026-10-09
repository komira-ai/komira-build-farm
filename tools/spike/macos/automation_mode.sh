#!/usr/bin/env bash
# P0 probe, the mechanics of `automationmodetool` status, enable and its control.
#
#   bash tools/spike/macos/automation_mode.sh
#
# Needs passwordless sudo (a hosted runner has it). With no arguments the tool reports
# two separate things, each parsed on its own: whether Automation Mode is enabled
# (`enabled`: on, off, unknown) and whether enabling it requires a user to
# authenticate (`auth`: required, not_required, unknown). The arms:
#
#   status_before  the status as the probe's own user;
#   control        `disable-automationmode-without-authentication` (the tool's only
#                  other verb: it brings the authentication requirement back, it does
#                  not switch Automation Mode off); the status must then say required.
#                  It runs first when the device starts out not_required, last
#                  otherwise, so the probe sees both transitions;
#   enable         `enable-automationmode-without-authentication` under a watchdog,
#                  run only while the last status said `required` (re-enabling an
#                  enabled device is reported to hang, so it is never retried and a
#                  watchdog firing is a FAIL); the status must then say not_required;
#   new_user       adds a throwaway standard user, reads the status as that user,
#                  deletes the user and reads it again: the setting must hold.
#
# Every automationmodetool and sysadminctl call runs under the watchdog
# (SPIKE_WATCHDOG seconds, default 60), with stdin closed: a first hosted run hung for
# 15 minutes in the user arm. The quick local reads (`id`, the context line) do not.
#
# The raw status text is recorded with every reading, so an unrecognised wording is
# visible in the log (and parses as unknown, which never triggers enable).

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=tools/spike/macos/lib.sh
. "$HERE/lib.sh"
PROBE=automation_mode
AMT=${SPIKE_AMT:-automationmodetool}
WATCHDOG=${SPIKE_WATCHDOG:-60}
PROBE_USER=${SPIKE_PROBE_USER:-kbfprobe1}

# am_parse TEXT: prints "<enabled> <auth>". "does not require" is matched before
# "requires": the second is not a substring of the first, but a looser pattern would be.
am_parse() {
    local t e=unknown a=unknown
    t=$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')
    case "$t" in
        *"automation mode is enabled"*) e=on ;;
        *"automation mode is disabled"*) e=off ;;
    esac
    case "$t" in
        *"does not require user authentication"*) a=not_required ;;
        *"requires user authentication"*) a=required ;;
    esac
    printf '%s %s' "$e" "$a"
}

# am_read LABEL [sudo -n -u USER]: reads the status (optionally as another user) and
# records LABEL.raw, LABEL.enabled and LABEL.auth; the auth also goes to state
# (LABEL and `last`).
am_read() {
    local label=$1 out parsed
    shift
    out=$(run_cap watchdog "$WATCHDOG" "$@" "$AMT")
    parsed=$(am_parse "$out")
    kv "$label.raw" "$out"
    kv "$label.enabled" "${parsed% *}"
    kv "$label.auth" "${parsed#* }"
    st_set "$label" "${parsed#* }"
    st_set last "${parsed#* }"
}

a_status_before() { am_read status_before; }

a_control() {
    local rc=0
    watchdog "$WATCHDOG" sudo -n "$AMT" disable-automationmode-without-authentication >/dev/null 2>&1 || rc=$?
    kv control.disable "rc=$rc"
    am_read after_control
    st_set control_ran yes
}

a_enable() {
    local rc=0
    if [ "$(st_get last)" != required ]; then
        kv enable "skipped: the last status was $(st_get last)"
        st_set enable skipped
        return 0
    fi
    watchdog "$WATCHDOG" sudo -n "$AMT" enable-automationmode-without-authentication >/dev/null 2>&1 || rc=$?
    if [ "$rc" -eq 124 ]; then
        kv enable "TIMEOUT after ${WATCHDOG}s"
        st_set enable timeout
        return 0
    fi
    kv enable "rc=$rc"
    st_set enable ran
    am_read after_enable
}

a_new_user() {
    local pw add
    # A throwaway password for a throwaway user that is deleted below.
    pw=$(od -An -N12 -tx1 /dev/urandom | tr -d ' \n')
    add=$(run_cap watchdog "$WATCHDOG" sudo -n sysadminctl -addUser "$PROBE_USER" -password "$pw")
    kv new_user.add "$add"
    case "$add" in *"[rc=124]") st_set user timeout ;; esac
    if id "$PROBE_USER" >/dev/null 2>&1; then kv new_user.exists yes; else kv new_user.exists no; fi
    am_read as_new_user sudo -n -u "$PROBE_USER"
    kv new_user.delete "$(run_cap watchdog "$WATCHDOG" sudo -n sysadminctl -deleteUser "$PROBE_USER")"
    if id "$PROBE_USER" >/dev/null 2>&1; then kv new_user.gone no; else kv new_user.gone yes; fi
    am_read after_user_delete
}

decide() {
    local before control enable after_enable as_user after_delete
    before=$(st_get status_before)
    control=$(st_get after_control)
    enable=$(st_get enable)
    after_enable=$(st_get after_enable)
    as_user=$(st_get as_new_user)
    after_delete=$(st_get after_user_delete)
    if [ "$before" = unknown ] || [ -z "$before" ]; then
        verdict FAIL "the status text was not recognised (see status_before.raw); enable was not run"
    elif [ "$control" != required ]; then
        verdict BROKEN "the control arm left auth=${control:-unread}, not required"
    elif [ "$enable" = timeout ]; then
        verdict FAIL "enable hung past the ${WATCHDOG}s watchdog"
    elif [ "$enable" != ran ]; then
        verdict FAIL "enable did not run (enable=${enable:-unset})"
    elif [ "$after_enable" != not_required ]; then
        verdict FAIL "after enable auth=$after_enable"
    elif [ "$(st_get user)" = timeout ]; then
        verdict FAIL "adding the user hung past the ${WATCHDOG}s watchdog"
    elif [ "$as_user" != not_required ] || [ "$after_delete" != not_required ]; then
        verdict FAIL "the setting did not hold across a new user (as user: $as_user, after delete: $after_delete)"
    else
        verdict PASS "enable took auth from required to not_required; it held for a new user and after its deletion; the control brought required back"
    fi
}

spike_context
arm status_before a_status_before
if [ "$(st_get status_before)" = not_required ]; then arm control a_control; fi
arm enable a_enable
arm new_user a_new_user
if [ "$(st_get control_ran)" != yes ]; then arm control a_control; fi
decide
