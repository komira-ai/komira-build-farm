#!/usr/bin/env bash
# Runs inside the Delegate=yes unit that tools/ci/podman-tests.sh starts, as the
# runner user. The unit's cgroup is left as systemd made it: the test process sets it
# up with the code kbf-daemon runs at start (crates/kbf-driver-container/src/
# delegate.rs), moving every process here (this one, cargo's, its own) into a
# `supervisor` leaf, then making `actions` with cpu, memory and pids enabled.
set -euo pipefail

cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
echo "delegated cgroup: ${cg#/sys/fs/cgroup} controllers: $(cat "$cg/cgroup.controllers")"
exec cargo test -p kbf-driver-container --test podman --locked -- --include-ignored
