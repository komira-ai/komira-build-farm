#!/usr/bin/env bash
# The M1 exit test: one integration cell, two pinned build tools, two builds each.
#
# The cell: an S3 store (MinIO in CI), one kbf-server (in-memory metadata, blobs in the
# store, REAPI on 127.0.0.1:8980, the worker listener on 127.0.0.1:8981 over mutual
# TLS) and one daemon (`kbf-cell daemon`: the daemon library with the test-only local
# runtime). For each tool given, the sample project under this directory is built
# remote-only, cleaned, and built again; `kbf-cell check` then requires that the first
# build ran every action on the farm and the second answered every one of them from
# the action cache. Last, the bucket must hold objects (the blobs went to the store).
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

"$bin_dir/kbf-server" --store s3 --s3-endpoint "$s3_endpoint" --s3-bucket "$s3_bucket" \
    --s3-conditional-put \
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

"$bin_dir/kbf-cell" daemon --server https://127.0.0.1:8981 --cas http://127.0.0.1:8980 \
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

if [ -n "$buck2" ]; then
    # `clean` stops buck2's daemon too, so the rebuild starts with no memory of the first.
    clean=("$buck2" clean)
    two_builds buck2 "$here/buck2" "$buck2" build --console simple //...
    (cd "$here/buck2" && "$buck2" kill)
fi

# The blobs are in the store, not only in the server's memory: the bucket holds objects.
objects=$(printf 'user = "%s:%s"\n' "$AWS_ACCESS_KEY_ID" "$AWS_SECRET_ACCESS_KEY" |
    curl -fsS -K - --aws-sigv4 "aws:amz:us-east-1:s3" "$s3_endpoint/$s3_bucket?list-type=2" |
    grep -o '<Key>' | wc -l)
[ "$objects" -gt 0 ] || { echo "FAIL: the bucket $s3_bucket holds no object" >&2; exit 1; }
echo "OK: the bucket $s3_bucket holds $objects object(s)"
