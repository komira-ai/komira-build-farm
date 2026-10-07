#!/usr/bin/env bash
# Runs inside the Delegate=yes unit that tools/ci/podman-tests.sh starts, as the
# runner user. A cgroup that enables controllers for children may hold no process
# (cgroup v2's no-internal-process rule), so this shell moves into a leaf first, then
# makes `actions` (the daemon's delegated subtree) with cpu, memory and pids enabled.
set -euo pipefail

cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
echo "delegated cgroup: ${cg#/sys/fs/cgroup} controllers: $(cat "$cg/cgroup.controllers")"
mkdir "$cg/supervisor"
echo $$ >"$cg/supervisor/cgroup.procs"
echo "+cpu +memory +pids" >"$cg/cgroup.subtree_control"
mkdir "$cg/actions"
echo "+cpu +memory +pids" >"$cg/actions/cgroup.subtree_control"

export KBF_TEST_CGROUP=${cg#/sys/fs/cgroup}/actions
exec cargo test -p kbf-driver-container --test podman --locked -- --include-ignored
