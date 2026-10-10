#!/usr/bin/env bash
# Runs the container driver's real-Podman tests (crates/kbf-driver-container/tests/
# podman.rs, podman_memory.rs and podman_env.rs) on a GitHub-hosted runner, set up the
# way the T4 spike found works (docs/spikes/hosted-runners.md):
#
# - a systemd user session for the runner user (lingering), so rootless Podman has a
#   run directory;
# - busybox pulled once by its image index digest, so the store holds both the index
#   and this architecture's manifest; the manifest's digest is read from the registry's
#   index (Podman's own `.Digest` is the digest of the first pull, here the index);
# - the tests run inside a system unit with Delegate=yes, as the runner user: the
#   user manager alone gets no io controller and a daemon must own its subtree. The
#   tests set the unit's cgroup up with kbf-daemon's own code (a `supervisor` leaf,
#   then `actions` with cpu, memory and pids; tools/ci/podman-tests-inner.sh).
#   The unit lowers its OOM score as the daemon's does (docs/deploy/
#   linux-build-host.md), so the tests see what an action inherits from it.
#
# The test binary is built before the unit starts, so the unit only runs it.
set -euo pipefail
cd "$(dirname "$0")/../.."

# busybox 1.37.0, image index digest (the one tools/spike/lib.sh pins).
BUSYBOX=docker.io/library/busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e

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
podman pull -q "$BUSYBOX" >/dev/null
case $(uname -m) in
x86_64) arch=amd64 ;;
aarch64) arch=arm64 ;;
*) echo "no busybox architecture for $(uname -m)" >&2; exit 1 ;;
esac
manifest=$(podman manifest inspect "$BUSYBOX" | python3 -I -c '
import json, sys
index = json.load(sys.stdin)
arch = sys.argv[1]
print(next(m["digest"] for m in index["manifests"]
           if m["platform"]["os"] == "linux" and m["platform"]["architecture"] == arch))
' "$arch")
case $manifest in
sha256:*) ;;
*) echo "no $arch manifest in $BUSYBOX: $manifest" >&2; exit 1 ;;
esac
if [ "docker.io/library/busybox@$manifest" = "$BUSYBOX" ]; then
    echo "the manifest digest equals the index digest; the index test would mean nothing" >&2
    exit 1
fi
echo "busybox manifest for $arch: $manifest"

cargo test -p kbf-driver-container --test podman --test podman_memory --test podman_env --locked --no-run

sudo systemd-run --quiet --wait --collect --pipe --unit=kbf-podman-tests \
    --slice=kbf-daemon.slice -p Delegate=yes -p OOMScoreAdjust=-900 \
    --uid="$uid" --gid="$(id -g)" --working-directory="$PWD" \
    -E HOME="$HOME" -E PATH="$PATH" -E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    -E CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-}" -E RUSTFLAGS="${RUSTFLAGS:-}" \
    -E KBF_TEST_IMAGE="docker://docker.io/library/busybox@$manifest" \
    -E KBF_TEST_INDEX_IMAGE="docker://$BUSYBOX" \
    bash tools/ci/podman-tests-inner.sh
