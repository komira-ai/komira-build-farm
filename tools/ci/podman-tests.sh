#!/usr/bin/env bash
# Runs the container driver's real-Podman tests (crates/kbf-driver-container/tests/
# podman.rs) on a GitHub-hosted runner, set up the way the T4 spike found works
# (docs/spikes/hosted-runners.md):
#
# - a systemd user session for the runner user (lingering), so rootless Podman has a
#   run directory;
# - busybox pulled once by its image index digest; the per-architecture manifest
#   digest is read back from the local store;
# - the tests run inside a system unit with Delegate=yes, as the runner user: the
#   user manager alone gets no io controller and a daemon must own its subtree. The
#   unit moves itself into a leaf and enables cpu, memory and pids for `actions`
#   (tools/ci/podman-tests-inner.sh).
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
manifest=$(podman image inspect --format '{{.Digest}}' "$BUSYBOX")
case $manifest in
sha256:*) ;;
*) echo "no manifest digest for $BUSYBOX: $manifest" >&2; exit 1 ;;
esac
if [ "docker.io/library/busybox@$manifest" = "$BUSYBOX" ]; then
    echo "the store reports the index digest as the manifest digest; the index test would mean nothing" >&2
    exit 1
fi
echo "busybox manifest for $(uname -m): $manifest"

cargo test -p kbf-driver-container --test podman --locked --no-run

sudo systemd-run --quiet --wait --collect --pipe --unit=kbf-podman-tests \
    --slice=kbf-daemon.slice -p Delegate=yes \
    --uid="$uid" --gid="$(id -g)" --working-directory="$PWD" \
    -E HOME="$HOME" -E PATH="$PATH" -E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    -E CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-}" -E RUSTFLAGS="${RUSTFLAGS:-}" \
    -E KBF_TEST_IMAGE="docker://docker.io/library/busybox@$manifest" \
    -E KBF_TEST_INDEX_IMAGE="docker://$BUSYBOX" \
    bash tools/ci/podman-tests-inner.sh
