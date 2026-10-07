#!/usr/bin/env bash
# Cgroup delegation as the daemon would use it: a system unit in kbf-daemon.slice with
# Delegate=yes, running as the unprivileged runner user. Inside it
# (delegated_inner.sh): which controllers arrive, whether the unit can move itself to
# a leaf and enable cpu/memory/io/pids for an `actions` subtree, whether io.max can be
# written there, and whether rootless Podman with --cgroup-parent puts a container
# under that subtree with its limits applied.
#
# For comparison, the controllers systemd gives the runner's user manager (the rootless
# path without a system unit) are recorded by host_facts.sh and podman_rootless.sh.
. "$(dirname "$0")/lib.sh"
user_session

read -r _ dmm <<<"$(disk_of "$SPIKE_TMP")"
out=$SPIKE_TMP/delegated.out
sudo systemd-run --quiet --wait --collect --unit=kbf-spike-daemon --slice=kbf-daemon.slice \
    -p Delegate=yes --uid="$(id -u)" --gid="$(id -g)" \
    -E HOME="$HOME" -E PATH="$PATH" -E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    -E SPIKE_BUSYBOX="$SPIKE_BUSYBOX" -E SPIKE_DISK="$dmm" -E RUNNER_TEMP="${RUNNER_TEMP:-}" \
    -p StandardOutput="file:$out" -p StandardError="file:$out.err" \
    bash "$SPIKE_DIR/delegated_inner.sh" || kv deleg_unit "FAILED($?)"
cat "$out"  # SPIKE lines plus any diagnostics
if [ -s "$out.err" ]; then
    echo "--- stderr of the delegated unit (first 20 lines)"
    head -n 20 "$out.err"
fi
grep -q "deleg_podman_leaf=" "$out" || { echo "the delegated unit recorded nothing" >&2; exit 1; }
