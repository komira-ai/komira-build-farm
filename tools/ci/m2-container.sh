#!/usr/bin/env bash
# Runs the M2 test (crates/kbf-it/m2/run.sh: buck2 through kbf-server to kbf-daemon
# --driver container) on a GitHub-hosted x86_64 runner, set up as
# tools/ci/podman-tests.sh sets one up for the driver's own Podman tests: a systemd
# user session for the runner user (lingering), so rootless Podman has a run
# directory. run.sh starts the daemon as a Delegate=yes system service itself.
#
# Usage: m2-container.sh BUCK2 WORK. The binaries are in target/debug.
set -euo pipefail
cd "$(dirname "$0")/../.."
buck2=$1 work=$2

uid=$(id -u)
if [ ! -S "/run/user/$uid/bus" ]; then
    sudo loginctl enable-linger "$(id -un)"
    for _ in $(seq 100); do
        [ -S "/run/user/$uid/bus" ] && break
        sleep 0.1
    done
fi
export XDG_RUNTIME_DIR=/run/user/$uid

podman --version
"$buck2" --version
# The swap the own-limit kill of //:hog is pushed into.
swapon --show
free -m
crates/kbf-it/m2/run.sh --bin-dir target/debug --work "$work" --buck2 "$buck2"
