#!/usr/bin/env bash
# Runs inside the Delegate=yes unit that tools/ci/podman-tests.sh starts, as the
# runner user. The unit's cgroup is left as systemd made it: the test process sets it
# up with the code kbf-daemon runs at start (crates/kbf-driver-container/src/
# delegate.rs), moving every process here (this one, cargo's, its own) into a
# `supervisor` leaf, then making `actions` with cpu, memory and pids enabled. It runs
# tests/podman.rs and tests/podman_memory.rs, then tests/podman_env.rs under a containers.conf override; the
# second finds this shell in `supervisor` already and sets up the same cgroup again.
set -euo pipefail

cg=/sys/fs/cgroup$(sed -n 's/^0:://p' /proc/self/cgroup)
echo "delegated cgroup: ${cg#/sys/fs/cgroup} controllers: $(cat "$cg/cgroup.controllers")"
# Every binary runs even when one before it fails; the script fails if any did.
status=0
cargo test -p kbf-driver-container --test podman --locked -- --include-ignored || status=$?
cargo test -p kbf-driver-container --test podman_memory --locked -- --include-ignored || status=$?
# A node whose containers.conf sets its own environment and limits: the driver's
# flags must win over every one of them (tests/podman_env.rs).
CONTAINERS_CONF_OVERRIDE=$PWD/crates/kbf-driver-container/tests/fixtures/containers-override.conf \
    cargo test -p kbf-driver-container --test podman_env --locked -- --include-ignored || status=$?
exit "$status"
