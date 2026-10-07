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
# - both: io.max and io.latency together (the configuration under test);
# - cpuonly: only the CPU half of the hog, no I/O lines (how much of the delay is CPU);
# - bothw8: both, plus CPUWeight=1000 on the server slice and the io.max cap at an
#   eighth of the disk rate;
# - sepopen, sepboth: open and both, with the probe on its own ext4 filesystem (a loop
#   image on the same disk), so it shares the device but not the hog's journal.
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
cap8=$((mbps / 8))
[ "$cap8" -lt 5 ] && cap8=5
kv io_direct_write_mbps "$mbps"
kv io_cap_mbps "$cap"
kv io_cap8_mbps "$cap8"

# Slice names carry no dash: systemd reads a dash as nesting (a-b.slice sits inside
# a.slice), and io.latency only throttles siblings, so all four sit directly under the
# root, next to each other.
unit() { printf '[Slice]\n%s\n' "${2:-}" | sudo tee "/run/systemd/system/$1" >/dev/null; }
unit kbfsrv.slice
unit kbfact.slice
unit kbfsrvlat.slice "IODeviceLatencyTargetSec=$dev 5ms"
unit kbfactcap.slice "IOWriteBandwidthMax=$dev ${cap}M"
unit kbfactcap8.slice "IOWriteBandwidthMax=$dev ${cap8}M"
unit kbfsrvlatw.slice "IODeviceLatencyTargetSec=$dev 5ms
CPUWeight=1000"
sudo systemctl daemon-reload

# Keep every slice active (so its cgroup, and anything written to it, lives for the
# whole step) with an idle unit in each.
for sl in kbfsrv kbfact kbfsrvlat kbfactcap kbfactcap8 kbfsrvlatw; do
    sudo systemd-run --quiet --collect --unit="kbf-spike-keep-$sl" --slice="$sl.slice" sleep infinity
done

# cgpath UNIT: the unit's cgroup directory.
cgpath() { printf '/sys/fs/cgroup%s' "$(systemctl show -p ControlGroup --value "$1")"; }

wbytes() {
    awk -v d="$dmm" '$1 == d { for (i = 2; i <= NF; i++) if ($i ~ /^wbytes=/) { sub("wbytes=", "", $i); print $i } }' \
        "$(cgpath "$1")/io.stat" 2>/dev/null || true
}

# What systemd wrote for the io.latency target, read back raw. If the kernel offers
# io.latency but systemd left it empty, write the target directly (recorded).
for sl in kbfsrvlat kbfsrvlatw; do
    f=$(cgpath "$sl.slice")/io.latency
    kv "io_latency_file_$sl" "$(if [ -e "$f" ]; then printf 'present: [%s]' "$(cat "$f")"; else echo absent; fi)"
    if [ -e "$f" ] && ! grep -q "^$dmm" "$f"; then
        kv "io_latency_direct_write_$sl" "$(try sudo sh -c "echo '$dmm target=5000' > $f") now [$(cat "$f")]"
    fi
done
kv io_files_kbfsrvlat "$(cd "$(cgpath kbfsrvlat.slice)" && ls -d io.* | tr '\n' ' ')"
kv io_root_subtree_control "$(cat /sys/fs/cgroup/cgroup.subtree_control)"

# A separate filesystem for the probe: same disk, own journal.
img=$SPIKE_TMP/raftfs.img
sepdir=$SPIKE_TMP/raftfs
sudo truncate -s 2G "$img"
sudo mkfs.ext4 -q -F "$img"
sudo mkdir -p "$sepdir"
sudo mount -o loop "$img" "$sepdir"
kv io_sepfs "$(findmnt -no SOURCE,FSTYPE "$sepdir" | sed 's|^/dev/||')"

# arm SERVER-SLICE [ACTIONS-SLICE] [HOG-MODE] [PROBE-DIR]: prints the probe line plus the hog's write rate.
# The io.max and io.latency files are read back (to stderr) while both run.
arm() {
    local server=$1 actions=${2:-} mode=${3:-all} pdir=${4:-$dir} w0=0 w1=0 res
    if [ -n "$actions" ]; then
        sudo systemd-run --quiet --collect --unit=kbf-spike-hog --slice="$actions" \
            bash "$SPIKE_DIR/io_hog.sh" "$dir" "$mode"
        sleep 3
        w0=$(wbytes "$actions")
    fi
    sudo systemd-run --quiet --wait --pipe --collect --unit=kbf-spike-probe --slice="$server" \
        python3 "$SPIKE_DIR/fsync_probe.py" "$pdir" "$secs" >"$SPIKE_TMP/probe.out" &
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

arms="baseline open cpuonly maxonly latonly both bothw8 sepopen sepboth"
declare -A worst best
for r in $(seq "$rounds"); do
    for a in $arms; do
        case $a in
            baseline) line=$(arm kbfsrv.slice) ;;
            open) line=$(arm kbfsrv.slice kbfact.slice) ;;
            maxonly) line=$(arm kbfsrv.slice kbfactcap.slice) ;;
            latonly) line=$(arm kbfsrvlat.slice kbfact.slice) ;;
            both) line=$(arm kbfsrvlat.slice kbfactcap.slice) ;;
            cpuonly) line=$(arm kbfsrv.slice kbfact.slice cpu) ;;
            bothw8) line=$(arm kbfsrvlatw.slice kbfactcap8.slice) ;;
            sepopen) line=$(arm kbfsrv.slice kbfact.slice all "$sepdir") ;;
            sepboth) line=$(arm kbfsrvlat.slice kbfactcap.slice all "$sepdir") ;;
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

# Verdict per guarded configuration: the test fits a hosted runner only if every round
# of that configuration holds the target and every open round (the mutant: both lines
# dropped) breaks it.
broke=$(python3 -c "print(${best[open]} >= $target_ms)")
for g in both bothw8 sepboth; do
    held=$(python3 -c "print(${worst[$g]} < $target_ms)")
    if [ "$held" = True ] && [ "$broke" = True ]; then
        v="fits: guarded p99 <= ${worst[$g]} ms < $target_ms ms; lines dropped, p99 >= ${best[open]} ms"
    else
        v="does not separate (guarded held: $held, open broke: $broke)"
    fi
    kv "io_verdict_$g" "$v"
done
