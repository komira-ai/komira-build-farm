#!/usr/bin/env bash
# PROBE (do not merge): what rootless Podman's store says about an image pulled by an
# index digest versus by a per-architecture manifest digest.
set -uo pipefail
cd "$(dirname "$0")/../.."
REPO=docker.io/library/busybox
BUSYBOX=$REPO@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
uid=$(id -u)
if [ ! -S "/run/user/$uid/bus" ]; then
    sudo loginctl enable-linger "$(id -un)"
    for _ in $(seq 100); do [ -S "/run/user/$uid/bus" ] && break; sleep 0.1; done
fi
export XDG_RUNTIME_DIR=/run/user/$uid
podman --version
skopeo --version || echo "PROBE no skopeo"
arch=$(uname -m); case $arch in x86_64) arch=amd64 ;; aarch64) arch=arm64 ;; esac
m=$(podman manifest inspect "$BUSYBOX" | python3 -c "
import json, sys
d = json.load(sys.stdin)
print([x['digest'] for x in d['manifests'] if x['platform']['architecture'] == '$arch' and x['platform']['os'] == 'linux'][0])")
echo "PROBE per-arch manifest: $m"
show() {
    echo "PROBE inspect $1: $(podman image inspect --format '{{.Id}} Digest={{.Digest}} RepoDigests={{json .RepoDigests}} ManifestType={{.ManifestType}}' "$1" 2>&1)"
}
storage() {
    local id gr drv
    id=$(podman image inspect --format '{{.Id}}' "$1")
    gr=$(podman info --format '{{.Store.GraphRoot}}')
    drv=$(podman info --format '{{.Store.GraphDriverName}}')
    echo "PROBE storage $gr $drv"
    for f in "$gr/$drv-images/$id"/*; do
        n=$(basename "$f")
        case $n in =*) echo "PROBE bigdata $(printf '%s' "${n#=}" | base64 -d 2>/dev/null) size=$(stat -c %s "$f") sha=$(sha256sum <"$f" | cut -c1-16) head=$(head -c 120 "$f" | tr '\n' ' ')" ;; *) echo "PROBE file $n" ;; esac
    done
}
echo "=== pulled by manifest"
podman pull -q "$REPO@$m" >/dev/null
show "$REPO@$m"; show "$BUSYBOX"; storage "$REPO@$m"
skopeo inspect --raw "containers-storage:$REPO@$m" 2>&1 | head -c 200; echo
echo "=== also pulled by index"
podman pull -q "$BUSYBOX" >/dev/null
show "$REPO@$m"; show "$BUSYBOX"; storage "$BUSYBOX"
skopeo inspect --raw "containers-storage:$BUSYBOX" 2>&1 | head -c 200; echo
echo "=== fresh store, pulled by index only"
podman rmi -a -f >/dev/null
podman pull -q "$BUSYBOX" >/dev/null
show "$REPO@$m"; show "$BUSYBOX"; storage "$BUSYBOX"
exit 1
