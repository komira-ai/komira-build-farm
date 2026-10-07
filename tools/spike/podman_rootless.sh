#!/usr/bin/env bash
# Rootless Podman on a hosted runner: is it there, which cgroup manager and network
# backend does it use, do --memory/--cpus/--pids-limit reach the container's cgroup
# (read back from inside) under each cgroup manager, and how long do
# pulls and a MinIO start take.
#
# Control arms (each can fail the step):
# - a memory hog over --memory must be OOM-killed, and the same command under the limit
#   must succeed, so "limits apply" is seen both ways;
# - MinIO must answer its health check, or the timing means nothing.
# Leaves the container `spike-minio` running for the later steps.
. "$(dirname "$0")/lib.sh"

uid=$(id -u)
if ! command -v podman >/dev/null; then
    t0=$(now_ms)
    sudo apt-get update -qq >/dev/null
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq podman >/dev/null
    kv podman_install_ms "$(($(now_ms) - t0))"
else
    kv podman_install_ms "0 (preinstalled)"
fi

info() {
    podman info --format json | python3 -c '
import json, sys
d = json.load(sys.stdin)["host"]
sec = d.get("security", {})
print(" ".join([
    "cgroupManager=" + str(d.get("cgroupManager")),
    "cgroupVersion=" + str(d.get("cgroupVersion")),
    "controllers=" + ",".join(d.get("cgroupControllers") or []),
    "rootless=" + str(sec.get("rootless")),
    "network=" + str(d.get("networkBackend")),
    "rootlessNetCmd=" + str(d.get("rootlessNetworkCmd")),
    "runtime=" + str(d.get("ociRuntime", {}).get("name")),
]))'
}

# limits LABEL GLOBAL-FLAG: run a container with limits and read them back from its
# own cgroup.
limits() {
    kv "podman_limits_$1" "$(try podman "$2" run --rm --memory 64m --memory-swap 64m --cpus 0.5 --pids-limit 64 "$SPIKE_BUSYBOX" \
        sh -c 'printf "memory.max=%s cpu.max=%s pids.max=%s" "$(cat /sys/fs/cgroup/memory.max)" "$(cat /sys/fs/cgroup/cpu.max)" "$(cat /sys/fs/cgroup/pids.max)"')"
}

# A systemd user session for the runner user (lingering starts user@<uid>.service,
# which brings the user bus Podman's systemd cgroup manager talks to). Set up before
# Podman first runs, so every step uses one storage and run root.
t0=$(now_ms)
user_session
kv user_session_ms "$(($(now_ms) - t0)) bus=$([ -S "/run/user/$uid/bus" ] && echo yes || echo no)"
kv cgroup_user_service_controllers "$(cat "/sys/fs/cgroup/user.slice/user-$uid.slice/user@$uid.service/cgroup.controllers" 2>/dev/null || echo absent)"
kv podman_info "$(try info)"

t0=$(now_ms)
podman pull -q "$SPIKE_BUSYBOX" >/dev/null
kv pull_busybox_ms "$(($(now_ms) - t0))"
t0=$(now_ms)
podman pull -q "$SPIKE_MINIO" >/dev/null
kv pull_minio_ms "$(($(now_ms) - t0))"
t0=$(now_ms)
podman pull -q "$SPIKE_RUSTFS" >/dev/null
kv pull_rustfs_ms "$(($(now_ms) - t0))"

# The limits under each cgroup manager: systemd (through the user manager) and cgroupfs
# (what Podman falls back to with no user session).
limits systemd --cgroup-manager=systemd
limits cgroupfs --cgroup-manager=cgroupfs

# Control: a 200 MiB buffer under a 64 MiB limit (no swap) is OOM-killed; 16 MiB is not.
podman rm -f spike-oom >/dev/null 2>&1 || true
podman run --name spike-oom --memory 64m --memory-swap 64m "$SPIKE_BUSYBOX" \
    dd if=/dev/zero of=/dev/null bs=200M count=1 >/dev/null 2>&1 || true
oom=$(podman inspect spike-oom --format '{{.State.OOMKilled}} exit={{.State.ExitCode}}')
podman rm -f spike-oom >/dev/null
under=$(try podman run --rm --memory 64m --memory-swap 64m "$SPIKE_BUSYBOX" dd if=/dev/zero of=/dev/null bs=16M count=1)
kv podman_oom_over_limit "$oom"
kv podman_under_limit "$(printf '%s' "$under" | tail -n 1)"
case $oom in
    true*) ;;
    *) echo "control failed: the hog over --memory was not OOM-killed ($oom)" >&2; exit 1 ;;
esac
case $under in
    FAILED*) echo "control failed: the same command under the limit failed ($under)" >&2; exit 1 ;;
esac

# MinIO, rootless, published on every host address (the no-network step needs a
# non-loopback address to aim at), timed from `run` to a healthy answer.
podman rm -f spike-minio >/dev/null 2>&1 || true
t0=$(now_ms)
podman run -d --name spike-minio -p 19000:9000 \
    -e MINIO_ROOT_USER=kbf-ci-access -e MINIO_ROOT_PASSWORD=kbf-ci-throwaway \
    "$SPIKE_MINIO" server /data >/dev/null
if wait_http http://127.0.0.1:19000/minio/health/live 60; then
    kv minio_ready_ms "$(($(now_ms) - t0))"
else
    kv minio_ready_ms "TIMEOUT"
    podman logs --tail 20 spike-minio >&2 || true
    exit 1
fi
kv minio_health_status "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:19000/minio/health/live)"
