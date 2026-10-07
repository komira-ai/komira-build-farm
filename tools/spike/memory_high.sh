#!/usr/bin/env bash
# memory.high on a hosted runner: a hog allocating 512 MiB under MemoryHigh=256M (no
# swap) runs beside four small actions in the same actions slice. The small actions
# must finish and the hog must still be alive (throttled, not killed) when they do.
#
# Arms:
# - baseline: the four small actions alone (their wall time);
# - high: the hog under memory.high, then the four small actions;
# - control: the same hog under memory.max instead must be OOM-killed. If it is not,
#   the hog never went over the limit and the high arm proves nothing: the step fails.
. "$(dirname "$0")/lib.sh"

slice=kbf-actions-mem.slice
printf '[Slice]\n' | sudo tee "/run/systemd/system/$slice" >/dev/null
sudo systemctl daemon-reload

smalls() {
    local i out=""
    for i in 1 2 3 4; do
        sudo systemd-run --quiet --wait --pipe --collect --slice="$slice" \
            python3 "$SPIKE_DIR/mem_hog.py" small >"$SPIKE_TMP/small-$i" &
    done
    wait
    for i in 1 2 3 4; do out="$out $(cat "$SPIKE_TMP/small-$i")"; done
    printf '%s' "${out# }"
}

psi_total() { sed -n 's/^some .*total=\([0-9]*\)/\1/p' /proc/pressure/memory; }

# hog UNIT LIMIT-PROPERTY: start the hog (512 MiB for 40 s) in its own unit.
hog() {
    sudo systemd-run --quiet --unit="$1" --slice="$slice" -p "$2" -p MemorySwapMax=0 \
        -p StandardOutput="file:$SPIKE_TMP/$1.out" \
        python3 "$SPIKE_DIR/mem_hog.py" hog 512 40
}

kv memhigh_baseline "$(smalls)"

p0=$(psi_total)
hog kbf-spike-memhog MemoryHigh=256M
sleep 5
kv memhigh_with_hog "$(smalls)"
cg=/sys/fs/cgroup/$slice/kbf-spike-memhog.service
kv memhigh_hog_state_after_smalls "$(systemctl show -p ActiveState --value kbf-spike-memhog)"
kv memhigh_hog_memory_current_mib "$(($(cat "$cg/memory.current") / 1048576))"
kv memhigh_hog_events "$(tr '\n' ' ' <"$cg/memory.events")"
kv memhigh_hog_pressure "$(head -n 1 "$cg/memory.pressure")"
kv memhigh_hog_progress "$(tail -n 1 "$SPIKE_TMP/kbf-spike-memhog.out")"
kv memhigh_host_psi_some_us "$(($(psi_total) - p0))"
sudo systemctl stop kbf-spike-memhog
kv memhigh_hog_result "$(systemctl show -p Result --value kbf-spike-memhog 2>/dev/null || echo gone)"
sudo systemctl reset-failed kbf-spike-memhog 2>/dev/null || true

# Control: memory.max must kill the same hog.
hog kbf-spike-memmax MemoryMax=256M
for _ in $(seq 60); do
    [ "$(systemctl show -p ActiveState --value kbf-spike-memmax)" = active ] || break
    sleep 0.5
done
result=$(systemctl show -p Result --value kbf-spike-memmax)
kv memmax_control_result "$result"
kv memmax_control_progress "$(tail -n 1 "$SPIKE_TMP/kbf-spike-memmax.out")"
sudo systemctl stop kbf-spike-memmax 2>/dev/null || true
sudo systemctl reset-failed kbf-spike-memmax 2>/dev/null || true
if [ "$result" != oom-kill ]; then
    echo "control failed: the hog under memory.max was not OOM-killed (Result=$result)" >&2
    exit 1
fi
