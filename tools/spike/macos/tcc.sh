#!/usr/bin/env bash
# P0 probe, TCC state as read: the Screen Recording, Accessibility, Input Monitoring
# and Post Event rows of the system and the user TCC database, read as the probe's
# user and as root, plus the SIP status. This records what a runner image seeded, so a
# hosted TCC or UI-test result is never trusted blind; it grants nothing.
#
#   bash tools/spike/macos/tcc.sh
#
# The arms:
#
#   sip       `csrutil status`;
#   control   the same reader over a database the probe plants with one known
#             ScreenCapture row; it must report exactly that row, or the probe is
#             BROKEN (a reader that prints nothing could not tell "no rows" from
#             "query wrong");
#   system / user, each as_user and as_root
#             readable yes/no, the row count per service, and the rows
#             (service|client|auth_value). An unreadable database is a fact (reading
#             one needs Full Disk Access unless SIP is off), not a failure.
#
# The verdict is PASS when the control row reads back; readability is reported in the
# arm lines.

HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=tools/spike/macos/lib.sh
. "$HERE/lib.sh"
PROBE=tcc
SYSTEM_DB=${SPIKE_TCC_SYSTEM_DB:-/Library/Application Support/com.apple.TCC/TCC.db}
USER_DB=${SPIKE_TCC_USER_DB:-$HOME/Library/Application Support/com.apple.TCC/TCC.db}
SERVICES="kTCCServiceScreenCapture kTCCServiceAccessibility kTCCServiceListenEvent kTCCServicePostEvent"
QUERY="SELECT service || '|' || client || '|' || auth_value FROM access WHERE service IN ('kTCCServiceScreenCapture', 'kTCCServiceAccessibility', 'kTCCServiceListenEvent', 'kTCCServicePostEvent') ORDER BY 1;"

# tcc_read LABEL DB [sudo -n]: records LABEL.readable, LABEL.<service> counts and
# LABEL.rows. Returns 1 when the database could not be read.
tcc_read() {
    local label=$1 db=$2 out rc=0 s n
    shift 2
    out=$("$@" sqlite3 -readonly "$db" "$QUERY" 2>&1) || rc=$?
    if [ "$rc" -ne 0 ]; then
        kv "$label.readable" "no: $(printf '%s' "$out" | head -n 1)"
        return 1
    fi
    kv "$label.readable" yes
    for s in $SERVICES; do
        n=$(printf '%s\n' "$out" | grep -c "^$s|" || true)
        kv "$label.${s#kTCCService}" "$n"
    done
    kv "$label.rows" "${out:-none}"
}

a_sip() { kv sip "$(run_cap csrutil status)"; }

a_control() {
    local db=$SPIKE_STATE/tcc-control.db
    rm -f "$db"
    sqlite3 "$db" "CREATE TABLE access (service TEXT, client TEXT, auth_value INTEGER); INSERT INTO access VALUES ('kTCCServiceScreenCapture', 'kbf.probe.control', 2);"
    tcc_read control "$db"
    if [ "$(grep -F "SPIKE $SPIKE_ARCH.tcc.control.rows=" "$SPIKE_RESULTS/spike.txt" | tail -n 1)" = \
        "SPIKE $SPIKE_ARCH.tcc.control.rows=kTCCServiceScreenCapture|kbf.probe.control|2" ]; then
        st_set control ok
    fi
}

a_system_user() { tcc_read system.as_user "$SYSTEM_DB" || true; }
a_system_root() { tcc_read system.as_root "$SYSTEM_DB" sudo -n || true; }
a_user_user() { tcc_read user.as_user "$USER_DB" || true; }
a_user_root() { tcc_read user.as_root "$USER_DB" sudo -n || true; }

spike_context
arm sip a_sip
arm control a_control
arm system.as_user a_system_user
arm system.as_root a_system_root
arm user.as_user a_user_user
arm user.as_root a_user_root
if [ "$(st_get control)" = ok ]; then
    verdict PASS "the reader returned the planted control row; the arm lines say what this image seeded"
else
    verdict BROKEN "the reader did not return the planted control row"
fi
