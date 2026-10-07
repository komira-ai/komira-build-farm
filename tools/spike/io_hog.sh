#!/usr/bin/env bash
# The actions-side hog for io_slices.sh: one CPU spinner per core, two direct-I/O
# writers and one buffered writer that syncs, all on DIR, until the unit is stopped.
# Usage: io_hog.sh DIR
set -u
dir=$1
for _ in $(seq "$(nproc)"); do
    sha256sum /dev/zero &
done
for i in 1 2; do
    while :; do dd if=/dev/zero of="$dir/hog-direct-$i" bs=1M count=512 oflag=direct status=none; done &
done
while :; do dd if=/dev/zero of="$dir/hog-buffered" bs=1M count=512 conv=fdatasync status=none; done &
wait
