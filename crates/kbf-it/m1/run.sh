#!/usr/bin/env bash
# The M1 exit test: one integration cell, two pinned build tools, two builds each.
#
# The cell: an S3 store (MinIO in CI), one kbf-server (in-memory metadata, blobs in the
# store, REAPI on 127.0.0.1:8980, the worker listener on 127.0.0.1:8981 over mutual
# TLS, which also serves the daemon its blobs) and one daemon (`kbf-cell daemon`: the
# daemon library with the test-only local runtime). For each tool given, the sample
# project under this directory is built remote-only, cleaned, and built again;
# `kbf-cell check` then requires that the first
# build ran every action on the farm and the second answered every one of them from
# the action cache. With buck2, one more build holds a lease in flight: the target
# `//:pause` sleeps for a few seconds under a fresh salt (so it runs; see the BUCK
# file), and while it runs the node is drained through the operator API. The drain
# must answer `draining` and list exactly the lease the daemon started; the build must
# take at least the sleep; the node must then be `drained`, and an uncordon returns it
# to `serving`. Last, the bucket must hold objects (the blobs went to the store).
# Any failure exits non-zero, after printing the cell's logs.
#
# Usage:
#   run.sh --bin-dir DIR --work DIR --s3-endpoint URL --s3-bucket NAME \
#          [--bazel PATH] [--buck2 PATH]
# The store's key pair comes from AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY. The
# bucket is created; it must not exist yet.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
bin_dir='' work='' s3_endpoint='' s3_bucket='' bazel='' buck2=''
while [ $# -gt 0 ]; do
    case $1 in
        --bin-dir) bin_dir=$2 ;;
        --work) work=$2 ;;
        --s3-endpoint) s3_endpoint=$2 ;;
        --s3-bucket) s3_bucket=$2 ;;
        --bazel) bazel=$2 ;;
        --buck2) buck2=$2 ;;
        *) echo "run.sh: unknown flag $1" >&2; exit 2 ;;
    esac
    shift 2
done
for flag in bin_dir work s3_endpoint s3_bucket; do
    [ -n "${!flag}" ] || { echo "run.sh: --${flag//_/-} is required" >&2; exit 2; }
done
[ -n "$bazel$buck2" ] || { echo "run.sh: give --bazel, --buck2 or both" >&2; exit 2; }
: "${AWS_ACCESS_KEY_ID:?the store key pair is required}" "${AWS_SECRET_ACCESS_KEY:?}"

mkdir -p "$work"
work=$(cd "$work" && pwd)
logs=$work/logs
mkdir -p "$logs"
pids=()

stop() {
    local status=$?
    for pid in "${pids[@]}"; do
        kill -INT "$pid" 2>/dev/null || true
    done
    for pid in "${pids[@]}"; do
        wait "$pid" 2>/dev/null || true
    done
    if [ "$status" -ne 0 ]; then
        for log in server.err daemon.err; do
            echo "--- last lines of $log"
            tail -n 60 "$logs/$log" 2>/dev/null || true
        done
    fi
    exit "$status"
}
trap stop EXIT

# The bucket, signed with the key pair (read from the environment, never argv).
printf 'user = "%s:%s"\n' "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" |
    curl -fsS -K - --aws-sigv4 "aws:amz:us-east-1:s3" -X PUT "$s3_endpoint/$s3_bucket"
echo "bucket $s3_bucket created"

"$bin_dir/kbf-cell" pki --dir "$work/pki" >/dev/null
# The operator API's write token: 64 hex digits in a file only this user can read.
(umask 077 && od -An -N32 -tx1 /dev/urandom | tr -d ' \n' >"$work/api-token")
api=http://127.0.0.1:8982

"$bin_dir/kbf-server" --store s3 --s3-endpoint "$s3_endpoint" --s3-bucket "$s3_bucket" \
    --s3-conditional-put \
    --listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
    --worker-tls-cert "$work/pki/server.pem" --worker-tls-key "$work/pki/server.key" \
    --worker-client-ca "$work/pki/ca.pem" \
    --api-listen 127.0.0.1:8982 --api-token-file "$work/api-token" \
    >"$logs/server.out" 2>"$logs/server.err" &
pids+=($!)
for _ in $(seq 100); do
    grep -q ' reapi=' "$logs/server.out" && break
    sleep 0.1
done
grep ' reapi=' "$logs/server.out" || { echo "kbf-server did not start" >&2; exit 1; }

"$bin_dir/kbf-cell" daemon --server https://127.0.0.1:8981 --cas https://127.0.0.1:8981 \
    --ca-cert "$work/pki/ca.pem" --cert "$work/pki/client.pem" --key "$work/pki/client.key" \
    --scratch "$work/leases" \
    2>"$logs/daemon.err" &
pids+=($!)
for _ in $(seq 100); do
    grep -q 'welcomed' "$logs/daemon.err" && break
    sleep 0.1
done
grep -q 'welcomed' "$logs/daemon.err" || { echo "the daemon did not join" >&2; exit 1; }
echo "daemon joined"

# One tool: build, clean, build again, check. Each build's console goes to a log.
two_builds() {
    local tool=$1 dir=$2
    shift 2
    local build=("$@")
    (cd "$dir" && "${build[@]}") 2>&1 | tee "$logs/$tool-first.log"
    (cd "$dir" && "${clean[@]}")
    (cd "$dir" && "${build[@]}") 2>&1 | tee "$logs/$tool-second.log"
    "$bin_dir/kbf-cell" check --tool "$tool" \
        --first "$logs/$tool-first.log" --second "$logs/$tool-second.log"
}

if [ -n "$bazel" ]; then
    root=$work/bazel-root
    clean=("$bazel" --output_user_root="$root" clean)
    two_builds bazel "$here/bazel" "$bazel" --output_user_root="$root" build //...
    (cd "$here/bazel" && "$bazel" --output_user_root="$root" shutdown)
fi

# The daemon's log without colour codes: the `lease started` lines with their ids.
leases_started() {
    sed 's/\x1b\[[0-9;]*m//g' "$logs/daemon.err" | sed -n 's/.*lease started lease=\([0-9.]*\).*/\1/p'
}

# One operator API write: `POST /v1/nodes/<node>:<verb>` with the JSON body $2.
api_write() {
    printf 'header = "Authorization: Bearer %s"\n' "$(cat "$work/api-token")" |
        curl -fsS -K - -X POST -H 'Content-Type: application/json' --data "$2" \
            "$api/v1/nodes/$1"
}

# A lease held in flight: build `//:pause` (sleep $secs, a fresh salt) in the
# background, drain the node while its lease runs, and check what the server showed.
lease_in_flight() {
    local secs=5 salt before lease node answer state held start_ms end_ms
    salt=$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')
    before=$(leases_started | wc -l)
    (cd "$here/buck2" && "$buck2" build --console simple \
        -c kbf_m1.sleep_secs="$secs" -c kbf_m1.salt="$salt" //:pause) \
        >"$logs/buck2-pause.log" 2>&1 &
    local build=$!
    pids+=("$build")
    for _ in $(seq 600); do
        [ "$(leases_started | wc -l)" -gt "$before" ] && break
        sleep 0.1
    done
    lease=$(leases_started | sed -n "$((before + 1))p")
    [ -n "$lease" ] || { echo "FAIL: //:pause started no lease on the daemon" >&2; exit 1; }
    # The daemon logs `lease started` before it fetches the inputs and runs the sleep, and
    # this poll sees the line within a tenth of a second or so: from here, the build
    # still takes the whole sleep, less that lag (half a second allowed).
    start_ms=$(date +%s%3N)
    node=$(curl -fsS "$api/v1/nodes" | jq -r '.nodes[0].node_id')
    answer=$(api_write "$node:drain" '{"deadline_secs": 600}')
    state=$(jq -r '.placement.state' <<<"$answer")
    held=$(jq -r '.placement.leases // [] | join(",")' <<<"$answer")
    if [ "$state" != draining ] || [ "$held" != "$lease" ]; then
        echo "FAIL: drained while lease $lease ran, the server answered $answer" >&2
        exit 1
    fi
    echo "OK: the server held lease $lease in flight while draining $node"
    wait "$build" || { cat "$logs/buck2-pause.log" >&2; echo "FAIL: //:pause failed" >&2; exit 1; }
    end_ms=$(date +%s%3N)
    if [ $((end_ms - start_ms)) -lt $((secs * 1000 - 500)) ]; then
        echo "FAIL: //:pause ended $((end_ms - start_ms)) ms after its lease started, under its ${secs} s sleep" >&2
        exit 1
    fi
    state=$(curl -fsS "$api/v1/nodes" | jq -r '.nodes[0].placement.state')
    [ "$state" = drained ] || { echo "FAIL: after the lease, $node is $state, not drained" >&2; exit 1; }
    state=$(api_write "$node:uncordon" '' | jq -r '.placement.state')
    [ "$state" = serving ] || { echo "FAIL: uncordoned, $node is $state" >&2; exit 1; }
    echo "OK: //:pause ended $((end_ms - start_ms)) ms after its lease started; $node drained, then serving again"
}

if [ -n "$buck2" ]; then
    # `clean` stops buck2's daemon too, so the rebuild starts with no memory of the first.
    clean=("$buck2" clean)
    two_builds buck2 "$here/buck2" "$buck2" build --console simple //...
    lease_in_flight
    (cd "$here/buck2" && "$buck2" kill)
fi

# The blobs are in the store, not only in the server's memory: the bucket holds objects.
objects=$(printf 'user = "%s:%s"\n' "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" |
    curl -fsS -K - --aws-sigv4 "aws:amz:us-east-1:s3" "$s3_endpoint/$s3_bucket?list-type=2" |
    grep -o '<Key>' | wc -l)
[ "$objects" -gt 0 ] || { echo "FAIL: the bucket $s3_bucket holds no object" >&2; exit 1; }
echo "OK: the bucket $s3_bucket holds $objects object(s)"
