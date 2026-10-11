#!/usr/bin/env bash
# The M2 test: real buck2, through kbf-server, to kbf-daemon --driver container, every
# action in a rootless Podman container of a pinned distroless image.
#
# The cell: one kbf-server (in-memory metadata and blobs, REAPI on 127.0.0.1:8980, the
# worker listener on 127.0.0.1:8981 over mutual TLS, which also serves the daemon its
# blobs) and one kbf-daemon with the container driver, run as a systemd service with
# Delegate=yes as the calling user (sudo systemd-run), the way a build host runs it:
# it sets up its own cgroup (a `supervisor` leaf and `actions` beside it) and makes a
# lease cgroup under `actions` for each container.
#
# What is checked, in order; any failure exits non-zero after printing the cell's logs:
# 1. The image is pulled, by its linux/amd64 manifest digest, into this user's image
#    store before the daemon starts (nodes never pull at action time). As on a farm,
#    the node's pre-pull ($image below) and the client's `container-image` (the
#    `container_image` value in m2/buck2/.buckconfig) are separate settings: a
#    client that names no image, or one the node did not pull, fails its build.
# 2. The sample project (a copy, with the static `kbf-m2-act` from --bin-dir in
#    tools/) is built remote-only, cleaned, and built again; `kbf-cell check` then
#    requires that the first build ran every action on the farm and the second
#    answered every one from the action cache.
# 3. `//:hog`, built in step 2, passed its first memory cap: the server's log holds
#    exactly one requeue of it from 1 GiB booked to 2 GiB booked (the own-limit
#    kill's doubling), and its output is there, so the second run succeeded.
# 4. `//:pause`, under a fresh salt so it runs, holds a lease for its sleep: while
#    the build waits, Podman lists exactly one running container labelled with the
#    daemon's node id, and the build takes at least the sleep.
# 5. Afterwards no container so labelled is left and the lease scratch directory is
#    empty.
# Each buck2 build is bounded by --build-timeout seconds, so a farm that reruns an
# action without end fails here instead of at the job's timeout.
#
# Usage:
#   run.sh --bin-dir DIR --work DIR --buck2 PATH [--build-timeout SECS]
# --bin-dir holds kbf-server, kbf-daemon, kbf-cell and kbf-m2-act. Needs rootless
# Podman with this user's subordinate ids, systemd, and sudo for systemd-run and
# systemctl (tools/ci/m2-container.sh sets a hosted runner up).
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
bin_dir='' work='' buck2='' build_timeout=300
while [ $# -gt 0 ]; do
    case $1 in
        --bin-dir) bin_dir=$2 ;;
        --work) work=$2 ;;
        --buck2) buck2=$2 ;;
        --build-timeout) build_timeout=$2 ;;
        *) echo "run.sh: unknown flag $1" >&2; exit 2 ;;
    esac
    shift 2
done
for flag in bin_dir work buck2; do
    [ -n "${!flag}" ] || { echo "run.sh: --${flag//_/-} is required" >&2; exit 2; }
done
bin_dir=$(cd "$bin_dir" && pwd)
buck2=$(cd "$(dirname "$buck2")" && pwd)/$(basename "$buck2")

mkdir -p "$work"
work=$(cd "$work" && pwd)
logs=$work/logs
project=$work/project
leases=$work/leases
node=cell-node-1
unit=kbf-m2-daemon
mkdir -p "$logs" "$leases"
pids=()
unit_started=''

stop() {
    local status=$?
    [ -d "$project" ] && (cd "$project" && "$buck2" kill >/dev/null 2>&1 || true)
    if [ -n "$unit_started" ]; then
        sudo systemctl stop "$unit" || true
    fi
    for pid in "${pids[@]}"; do
        kill -INT "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
    if [ "$status" -ne 0 ]; then
        for log in server.err daemon.err; do
            echo "--- last lines of $log"
            tail -n 80 "$logs/$log" 2>/dev/null || true
        done
    fi
    exit "$status"
}
trap stop EXIT

# 1. The image: distroless base-debian12, its linux/amd64 manifest (not the index).
image=gcr.io/distroless/base-debian12@sha256:d2add786f2a5f43d1ab3ae54cd3193de929d0d12c378ff60891921f56f3e47ff
echo "OK: pulled $image"

# 2. The project copy and its static helper.
[ ! -e "$project" ] || { echo "run.sh: $project exists; give a fresh --work" >&2; exit 2; }
cp -r "$here/buck2" "$project"
mkdir -p "$project/tools"
cp "$bin_dir/kbf-m2-act" "$project/tools/kbf-m2-act"
if readelf -l "$project/tools/kbf-m2-act" | grep -q 'INTERP'; then
    echo "FAIL: kbf-m2-act is dynamically linked; the image may not hold its libraries" >&2
    exit 1
fi

"$bin_dir/kbf-cell" pki --dir "$work/pki" --node-id "$node" >/dev/null

"$bin_dir/kbf-server" \
    --listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
    --worker-tls-cert "$work/pki/server.pem" --worker-tls-key "$work/pki/server.key" \
    --worker-client-ca "$work/pki/ca.pem" \
    >"$logs/server.out" 2>"$logs/server.err" &
pids+=($!)
for _ in $(seq 100); do
    grep -q ' reapi=' "$logs/server.out" && break
    sleep 0.1
done
grep ' reapi=' "$logs/server.out" || { echo "kbf-server did not start" >&2; exit 1; }

# The daemon as a build host runs it: a service with its own delegated cgroup, as
# this user, its stderr appended to daemon.err. Its leases may use 8 GiB together.
: >"$logs/daemon.err"
sudo systemd-run --quiet --collect --unit="$unit" --slice=kbf-daemon.slice \
    -p Delegate=yes -p OOMScoreAdjust=-900 \
    -p StandardError="append:$logs/daemon.err" \
    --uid="$(id -u)" --gid="$(id -g)" --working-directory="$work" \
    -E HOME="$HOME" -E PATH="$PATH" -E XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
    "$bin_dir/kbf-daemon" --driver container \
    --server https://127.0.0.1:8981 --cas https://127.0.0.1:8981 \
    --ca-cert "$work/pki/ca.pem" --cert "$work/pki/client.pem" --key "$work/pki/client.key" \
    --tls-server-name localhost --node-id "$node" --scratch "$leases" \
    --actions-memory-max-gib 8
unit_started=1
for _ in $(seq 300); do
    grep -q 'welcomed' "$logs/daemon.err" && break
    sleep 0.1
done
grep -q 'welcomed' "$logs/daemon.err" || { echo "the daemon did not join" >&2; exit 1; }
echo "daemon joined"

build() {
    local log=$1
    shift
    if ! (cd "$project" && timeout "$build_timeout" "$buck2" build --console simple "$@") \
        >"$log" 2>&1; then
        cat "$log"
        echo "FAIL: buck2 build $* failed (or ran past $build_timeout s)" >&2
        exit 1
    fi
    cat "$log"
}

build "$logs/buck2-first.log" //...
(cd "$project" && "$buck2" clean)
build "$logs/buck2-second.log" //...
"$bin_dir/kbf-cell" check --tool buck2 \
    --first "$logs/buck2-first.log" --second "$logs/buck2-second.log"

# 3. The hog's doubling: one requeue from 1 GiB to 2 GiB, then its output.
requeues=$(sed 's/\x1b\[[0-9;]*m//g' "$logs/server.err" | grep -cF \
    'the action passed its memory limit with 1 GiB booked; it runs again with 2 GiB booked' ||
    true)
if [ "$requeues" -ne 1 ]; then
    echo "FAIL: the server requeued an action from 1 GiB to 2 GiB $requeues times, not once" >&2
    exit 1
fi
hog_out=$(cd "$project" && "$buck2" build --console none --show-full-simple-output //:hog)
[ "$(cat "$hog_out")" = 3072 ] || { echo "FAIL: //:hog's output $hog_out is not 3072" >&2; exit 1; }
echo "OK: //:hog was killed at its own 2 GiB cap, run again with 2 GiB booked, and passed"

# 4. A lease held in flight, in a container.
leases_started() {
    sed 's/\x1b\[[0-9;]*m//g' "$logs/daemon.err" | grep -c 'lease started' || true
}
containers() {
    podman ps "$@" --filter "label=kbf.owner=$node" --format '{{.Names}}' | grep -c . || true
}
secs=8
salt=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
before=$(leases_started)
start_ms=$(date +%s%3N)
(cd "$project" && timeout "$build_timeout" "$buck2" build --console simple \
    -c kbf_m2.sleep_secs="$secs" -c kbf_m2.salt="$salt" //:pause) \
    >"$logs/buck2-pause.log" 2>&1 &
pause_build=$!
pids+=("$pause_build")
running=0
for _ in $(seq 600); do
    if [ "$(leases_started)" -gt "$before" ]; then
        running=$(containers --filter status=running)
        [ "$running" -ge 1 ] && break
    fi
    sleep 0.1
done
if [ "$running" -ne 1 ]; then
    echo "FAIL: while //:pause ran, $running running containers were labelled kbf.owner=$node" >&2
    exit 1
fi
echo "OK: //:pause holds a lease in one running container"
wait "$pause_build" || { cat "$logs/buck2-pause.log" >&2; echo "FAIL: //:pause failed" >&2; exit 1; }
end_ms=$(date +%s%3N)
if [ $((end_ms - start_ms)) -lt $((secs * 1000)) ]; then
    echo "FAIL: //:pause took $((end_ms - start_ms)) ms, under its ${secs} s sleep" >&2
    exit 1
fi
echo "OK: //:pause took $((end_ms - start_ms)) ms"

# 5. Nothing left behind (a few seconds allowed for the clean step to end).
for _ in $(seq 100); do
    left=$(containers -a)
    [ "$left" -eq 0 ] && [ -z "$(ls -A "$leases")" ] && break
    sleep 0.1
done
[ "$left" -eq 0 ] || { echo "FAIL: $left containers labelled kbf.owner=$node are left" >&2; exit 1; }
if [ -n "$(ls -A "$leases")" ]; then
    echo "FAIL: the lease scratch directory is not empty: $(ls -A "$leases")" >&2
    exit 1
fi
echo "OK: no container and no lease directory left"
