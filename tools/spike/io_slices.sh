#!/usr/bin/env bash
# Converged slices on one hosted VM: does an io.max cap on the actions slice plus an
# io.latency target on the server slice keep a Raft-like fsync probe's p99 under
# 10 ms while a CPU and disk hog runs in the actions slice, and does the probe go over
# 10 ms when those lines are dropped? Only if both hold can the slice test (and its
# "drop io.max/io.latency" mutant) run on a hosted runner.
#
# Arms, interleaved over ROUNDS rounds so drift hits all of them alike:
# - baseline: the probe alone in the server slice;
# - open (the planted defect): the hog in the actions slice, no I/O lines;
# - maxonly: the hog under an io.max write cap at a quarter of the disk's measured
#   direct-write rate;
# - latonly: the probe's slice with an io.latency target of 5 ms;
# - both: io.max and io.latency together (the configuration under test).
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

# Slice names carry no dash: systemd reads a dash as nesting (a-b.slice sits inside
# a.slice), and io.latency only throttles siblings, so all four sit directly under the
# root, next to each other.
unit() { printf '[Slice]\n%s\n' "${2:-}" | sudo tee "/run/systemd/system/$1" >/dev/null; }
unit kbfsrv.slice
unit kbfact.slice
unit kbfsrvlat.slice "IODeviceLatencyTargetSec=$dev 5ms"
unit kbfactcap.slice "IOWriteBandwidthMax=$dev ${cap}M"
sudo systemctl daemon-reload

# cgpath UNIT: the unit's cgroup directory.
cgpath() { printf '/sys/fs/cgroup%s' "$(systemctl show -p ControlGroup --value "$1")"; }

wbytes() {
    awk -v d="$dmm" '$1 == d { for (i = 2; i <= NF; i++) if ($i ~ /^wbytes=/) { sub("wbytes=", "", $i); print $i } }' \
        "$(cgpath "$1")/io.stat" 2>/dev/null || true
}

# arm SERVER-SLICE [ACTIONS-SLICE]: prints the probe line plus the hog's write rate.
# The io.max and io.latency files are read back (to stderr) while both run.
arm() {
    local server=$1 actions=${2:-} w0=0 w1=0 res
    if [ -n "$actions" ]; then
        sudo systemd-run --quiet --collect --unit=kbf-spike-hog --slice="$actions" \
            bash "$SPIKE_DIR/io_hog.sh" "$dir"
        sleep 3
        w0=$(wbytes "$actions")
    fi
    sudo systemd-run --quiet --wait --pipe --collect --unit=kbf-spike-probe --slice="$server" \
        python3 "$SPIKE_DIR/fsync_probe.py" "$dir" "$secs" >"$SPIKE_TMP/probe.out" &
    sleep 2
    if [ -n "$actions" ]; then
        kv "io_readback_${actions%.slice}_io_max" "$(cat "$(cgpath "$actions")/io.max" 2>&1 | grep "^$dmm" || echo none)" >&2
    fi
    kv "io_readback_${server%.slice}_io_latency" "$(cat "$(cgpath "$server")/io.latency" 2>&1 | grep "^$dmm" || echo none)" >&2
    wait
    res=$(cat "$SPIKE_TMP/probe.out")
    if [ -n "$actions" ]; then
        w1=$(wbytes "$actions")
        sudo systemctl stop kbf-spike-hog
        res="$res hog_write_mbps=$(((${w1:-0} - ${w0:-0}) / 1048576 / secs))"
    fi
    sync
    sleep 2
    printf '%s' "$res"
}

arms="baseline open maxonly latonly both"
declare -A worst best
for r in $(seq "$rounds"); do
    for a in $arms; do
        case $a in
            baseline) line=$(arm kbfsrv.slice) ;;
            open) line=$(arm kbfsrv.slice kbfact.slice) ;;
            maxonly) line=$(arm kbfsrv.slice kbfactcap.slice) ;;
            latonly) line=$(arm kbfsrvlat.slice kbfact.slice) ;;
            both) line=$(arm kbfsrvlat.slice kbfactcap.slice) ;;
        esac
        kv "io_${a}_round$r" "$line"
        p99=$(printf '%s' "$line" | sed -n 's/.*p99_ms=\([0-9.]*\).*/\1/p')
        worst[$a]=$(python3 -c "print(max(${worst[$a]:-0}, ${p99:-1e9}))")
        best[$a]=$(python3 -c "print(min(${best[$a]:-1e9}, ${p99:-1e9}))")
    done
done
for a in $arms; do
    kv "io_${a}_p99_ms_range" "${best[$a]}..${worst[$a]}"
done

# Verdict: the test fits a hosted runner only if every round with both lines holds
# the target and every open round (the mutant: both lines dropped) breaks it.
held=$(python3 -c "print(${worst[both]} < $target_ms)")
broke=$(python3 -c "print(${best[open]} >= $target_ms)")
if [ "$held" = True ] && [ "$broke" = True ]; then
    v="fits: with both lines p99 <= ${worst[both]} ms < $target_ms ms; dropped, p99 >= ${best[open]} ms"
else
    v="does not separate (both held: $held, open broke: $broke)"
fi
kv io_verdict "$v"
