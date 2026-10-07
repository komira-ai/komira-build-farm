#!/usr/bin/env bash
# Converged slices on one hosted VM: does an io.max cap on the actions slice plus an
# io.latency target on the server slice keep a Raft-like fsync probe's p99 under
# 10 ms while a CPU and disk hog runs in the actions slice, and does the probe go over
# 10 ms when those lines are dropped? Only if both hold can the slice test (and its
# "drop io.max/io.latency" mutant) run on a hosted runner.
#
# Arms, interleaved over ROUNDS rounds so drift hits all three alike:
# - baseline: the probe alone in kbf-server.slice;
# - open (the planted defect): the hog in kbf-actions.slice, no I/O lines;
# - guarded: the hog in kbf-actions-capped.slice (io.max write cap at a quarter of
#   the disk's measured direct-write rate), the probe in kbf-server-guarded.slice
#   (io.latency target 5 ms).
# The slices are runtime unit files written under /run/systemd/system; the io.max and
# io.latency values systemd wrote are read back from cgroupfs.
. "$(dirname "$0")/lib.sh"

rounds=${SPIKE_IO_ROUNDS:-3}
secs=${SPIKE_IO_SECONDS:-15}
target_ms=10
dir=$SPIKE_TMP/io
sudo mkdir -p "$dir"
read -r dname dmm <<<"$(disk_of "$dir")"
dev=/dev/$dname
kv io_disk "$dname $dmm"

# The disk's direct-write rate, alone, sets the cap.
t0=$(now_ms)
sudo dd if=/dev/zero of="$dir/speed" bs=1M count=1024 oflag=direct status=none
ms=$(($(now_ms) - t0))
mbps=$((1024 * 1000 / ms))
sudo rm -f "$dir/speed"
cap=$((mbps / 4))
[ "$cap" -lt 5 ] && cap=5
kv io_direct_write_mbps "$mbps"
kv io_cap_mbps "$cap"

unit() { printf '[Slice]\n%s\n' "${2:-}" | sudo tee "/run/systemd/system/$1" >/dev/null; }
unit kbf-server.slice
unit kbf-actions.slice
unit kbf-server-guarded.slice "IODeviceLatencyTargetSec=$dev 5ms"
unit kbf-actions-capped.slice "IOWriteBandwidthMax=$dev ${cap}M"
sudo systemctl daemon-reload

wbytes() {
    awk -v d="$dmm" '$1 == d { for (i = 2; i <= NF; i++) if ($i ~ /^wbytes=/) { sub("wbytes=", "", $i); print $i } }' \
        "/sys/fs/cgroup/$1/io.stat" 2>/dev/null || true
}

# arm NAME SERVER-SLICE [ACTIONS-SLICE]: prints the probe line plus the hog's write rate.
arm() {
    local name=$1 server=$2 actions=${3:-} w0=0 w1=0 res
    if [ -n "$actions" ]; then
        sudo systemd-run --quiet --collect --unit=kbf-spike-hog --slice="$actions" \
            bash "$SPIKE_DIR/io_hog.sh" "$dir"
        sleep 3
        w0=$(wbytes "$actions")
    fi
    res=$(sudo systemd-run --quiet --wait --pipe --collect --slice="$server" \
        python3 "$SPIKE_DIR/fsync_probe.py" "$dir" "$secs")
    if [ -n "$actions" ]; then
        w1=$(wbytes "$actions")
        if [ "$name" = guarded ]; then
            kv io_readback_io_max "$(cat "/sys/fs/cgroup/$actions/io.max")" >&2
            kv io_readback_io_latency "$(cat "/sys/fs/cgroup/$server/io.latency" 2>&1)" >&2
        fi
        sudo systemctl stop kbf-spike-hog
        res="$res hog_write_mbps=$(((${w1:-0} - ${w0:-0}) / 1048576 / secs))"
    fi
    sync
    sleep 2
    printf '%s' "$res"
}

declare -A worst best
for r in $(seq "$rounds"); do
    for a in baseline open guarded; do
        case $a in
            baseline) line=$(arm baseline kbf-server.slice) ;;
            open) line=$(arm open kbf-server.slice kbf-actions.slice) ;;
            guarded) line=$(arm guarded kbf-server-guarded.slice kbf-actions-capped.slice) ;;
        esac
        kv "io_${a}_round$r" "$line"
        p99=$(printf '%s' "$line" | sed -n 's/.*p99_ms=\([0-9.]*\).*/\1/p')
        worst[$a]=$(python3 -c "print(max(${worst[$a]:-0}, $p99))")
        best[$a]=$(python3 -c "print(min(${best[$a]:-1e9}, $p99))")
    done
done
for a in baseline open guarded; do
    kv "io_${a}_p99_ms_range" "${best[$a]}..${worst[$a]}"
done

# Verdict: the test fits a hosted runner only if every guarded round holds the target
# and every open round (the mutant) breaks it.
held=$(python3 -c "print(${worst[guarded]} < $target_ms)")
broke=$(python3 -c "print(${best[open]} >= $target_ms)")
if [ "$held" = True ] && [ "$broke" = True ]; then
    v="fits: guarded p99 <= ${worst[guarded]} ms < $target_ms ms <= open p99 >= ${best[open]} ms"
else
    v="does not separate (guarded held: $held, open broke: $broke)"
fi
kv io_verdict "$v"
