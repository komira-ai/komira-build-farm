#!/usr/bin/env bash
# Runs inside the delegated unit started by delegation.sh (as the runner user). Prints
# SPIKE lines to the unit's stdout; delegation.sh copies them into the job log.
. "$(dirname "$0")/lib.sh"
GITHUB_STEP_SUMMARY=

cg=/sys/fs/cgroup$(self_cgroup)
kv deleg_cgroup "$(self_cgroup)"
kv deleg_owner "$(stat -c %U "$cg")"
kv deleg_controllers "$(cat "$cg/cgroup.controllers")"

# No internal processes: a cgroup that enables controllers for children must hold no
# process itself, so the supervisor moves into a leaf first.
mkdir "$cg/supervisor"
echo $$ >"$cg/supervisor/cgroup.procs"
for c in cpu memory io pids; do
    kv "deleg_enable_$c" "$(try sh -c "echo +$c > $cg/cgroup.subtree_control")"
done
mkdir "$cg/actions"
for c in cpu memory io pids; do
    kv "deleg_actions_enable_$c" "$(try sh -c "echo +$c > $cg/actions/cgroup.subtree_control")"
done
kv deleg_actions_subtree_control "$(cat "$cg/actions/cgroup.subtree_control")"

# io.max on the delegated subtree, for the disk holding the scratch directory.
mkdir "$cg/actions/iotest"
kv deleg_io_max_write "$(try sh -c "echo '$SPIKE_DISK wbps=10485760' > $cg/actions/iotest/io.max")"
kv deleg_io_max_read "$(cat "$cg/actions/iotest/io.max" 2>&1)"
rmdir "$cg/actions/iotest"

# Rootless Podman placing a container under the delegated subtree (cgroupfs manager:
# the daemon owns this subtree, not the user's systemd). The container's cgroup is
# found from the host side and its limits are read there.
parent=${cg#/sys/fs/cgroup}/actions
cid=$(podman --cgroup-manager=cgroupfs run -d --cgroup-parent="$parent" \
    --memory 64m --memory-swap 64m --cpus 0.5 --pids-limit 64 "$SPIKE_BUSYBOX" sleep 60 2>"$SPIKE_TMP/deleg-podman.err") || {
    kv deleg_podman_leaf "FAILED: $(tail -n 3 "$SPIKE_TMP/deleg-podman.err")"
    # Where did crun try to create the cgroup? Debug log lines naming cgroups, then
    # every directory under the delegated cgroup with its controllers.
    podman --log-level=debug --cgroup-manager=cgroupfs run --rm --cgroup-parent="$parent" \
        --memory 64m "$SPIKE_BUSYBOX" true 2>&1 | grep -i cgroup | head -n 15 || true
    find "$cg" -mindepth 1 -type d | while read -r d; do
        echo "dir ${d#"$cg"/} controllers=[$(cat "$d/cgroup.controllers")] subtree=[$(cat "$d/cgroup.subtree_control")]"
    done
    exit 0
}
kv deleg_podman_stderr "$(head -n 3 "$SPIKE_TMP/deleg-podman.err")"
sleep 1
leaf=$(find "$cg/actions" -mindepth 1 -type d -name "*${cid:0:12}*" | head -n 1)
if [ -n "$leaf" ]; then
    kv deleg_podman_leaf "${leaf#"$cg"/} memory.max=$(cat "$leaf/memory.max") cpu.max=$(cat "$leaf/cpu.max") pids.max=$(cat "$leaf/pids.max")"
else
    kv deleg_podman_leaf "NOT UNDER actions: $(podman inspect "$cid" --format '{{.State.CgroupPath}}' 2>&1)"
fi
podman rm -f "$cid" >/dev/null 2>&1 || true
