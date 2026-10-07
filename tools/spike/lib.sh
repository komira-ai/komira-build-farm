# Shared helpers for the hosted-runner spike scripts (sourced, not run).
#
# Every measurement is printed as one `SPIKE <arch>.<key>=<value>` line, so a run's
# numbers can be read back with `gh run view <id> --log | grep ' SPIKE '`. The same
# lines go to the job summary. Values never carry an address: the scripts print
# outcomes and counts, not `ip`, `resolv.conf` or network inspect output.

set -euo pipefail

SPIKE_ARCH=$(uname -m)
SPIKE_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
# Scratch for the probes: on the same filesystem as the workspace.
SPIKE_TMP=${RUNNER_TEMP:-$HOME}/kbf-spike
mkdir -p "$SPIKE_TMP"

# The images the spike pulls, each by digest. MinIO and RustFS are the digests ci.yml
# already uses for its object-store job; busybox is 1.37.0 (image index digest).
SPIKE_MINIO=cgr.dev/chainguard/minio@sha256:e7ca559d9f7c0b5f24f5f669bb92f40f3ca88d56273b808bf3a7c116c17d2ffa
SPIKE_RUSTFS=docker.io/rustfs/rustfs@sha256:1803faef57627e2d9c2e7d89d655d712ddded5389040054987163043fecb6a3c
SPIKE_BUSYBOX=docker.io/library/busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e

# kv KEY VALUE: one measurement line (newlines in VALUE are folded to " | ").
kv() {
    local v=${2//$'\n'/ | }
    printf 'SPIKE %s.%s=%s\n' "$SPIKE_ARCH" "$1" "$v"
    if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
        printf -- '- `%s.%s` = `%s`\n' "$SPIKE_ARCH" "$1" "$v" >>"$GITHUB_STEP_SUMMARY"
    fi
}

now_ms() { date +%s%3N; }

# try CMD...: run CMD, print its output (stdout and stderr, first 3 lines) or
# "FAILED(<status>): <output>". Never fails the caller.
try() {
    local out rc=0
    out=$("$@" 2>&1) || rc=$?
    out=$(printf '%s' "$out" | head -n 3)
    if [ "$rc" -eq 0 ]; then printf '%s' "$out"; else printf 'FAILED(%s): %s' "$rc" "$out"; fi
}

# wait_http URL SECONDS: 0 once URL answers any HTTP status, 1 on timeout.
wait_http() {
    local url=$1 deadline=$(($(date +%s) + $2))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if curl -s -o /dev/null --max-time 2 "$url"; then return 0; fi
        sleep 0.2
    done
    return 1
}

# The cgroup v2 path (under /sys/fs/cgroup) of this shell.
self_cgroup() { sed -n 's/^0:://p' /proc/self/cgroup; }

# The whole-disk block device holding PATH: prints "<name> <maj:min>".
disk_of() {
    local mm sys
    mm=$(mountpoint -d "$(findmnt -no TARGET --target "$1")")
    sys=$(readlink -f "/sys/dev/block/$mm")
    if [ -f "$sys/partition" ]; then sys=$(dirname "$sys"); fi
    printf '%s %s' "$(basename "$sys")" "$(cat "$sys/dev")"
}

# user_session: make sure the runner user has a systemd user manager (lingering) and
# export the variables rootless Podman reads, so every step sees the same Podman
# storage and run root. Prints nothing; the Podman step records the timing.
user_session() {
    local uid
    uid=$(id -u)
    if [ ! -S "/run/user/$uid/bus" ]; then
        sudo loginctl enable-linger "$(id -un)"
        for _ in $(seq 100); do
            [ -S "/run/user/$uid/bus" ] && break
            sleep 0.1
        done
    fi
    export XDG_RUNTIME_DIR=/run/user/$uid
    export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/$uid/bus
}
