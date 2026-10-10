#!/usr/bin/env bash
# Time an integration job pays before its first test: the pinned Rust toolchain, a cold
# `cargo build --workspace --locked` (no cache restored), three kbf-server processes
# started at once from the result and stopped again, and a pinned buck2 release fetched
# and checked by sha256. Bazel's presence is recorded (the images ship bazelisk).
#
# Controls: the step fails if a kbf-server exits or prints no start line within 60 s,
# if one is still running 30 s after SIGTERM or exits non-zero after it, or if the
# buck2 download does not match its pinned sha256.
. "$(dirname "$0")/lib.sh"

t0=$(now_ms)
rustup toolchain install >/dev/null 2>&1
kv build_toolchain_install_ms "$(($(now_ms) - t0))"

t0=$(now_ms)
cargo build --workspace --locked --quiet
kv build_cold_workspace_ms "$(($(now_ms) - t0))"
kv build_target_dir_mib "$(du -sm target | cut -f1)"

# kbf-server serves until SIGINT or SIGTERM. Each of the three binds free ports (port
# 0), so they never collide, and is timed from the launch to its start line
# (`kbf-server <version> reapi=<addr> worker=<addr>`). Then all three get SIGTERM and
# must exit 0 within the bound; with no client connected none of them waits for its
# --shutdown-timeout-secs (10 s by default).
server_start_s=60
server_stop_s=30
fail() { echo "build_budget.sh: $*" >&2; exit 1; }
# server_log N: what server N printed (stdout, then stderr), for a failure message.
server_log() { cat "$SPIKE_TMP/server$1.out" "$SPIKE_TMP/server$1.err" 2>/dev/null | head -n 5; }
pids=()
# Never leave a server behind, whatever stops the script.
trap 'for p in "${pids[@]}"; do kill -KILL "$p" 2>/dev/null || true; done' EXIT

t0=$(now_ms)
for i in 0 1 2; do
    target/debug/kbf-server --listen 127.0.0.1:0 --worker-listen 127.0.0.1:0 \
        >"$SPIKE_TMP/server$i.out" 2>"$SPIKE_TMP/server$i.err" &
    pids+=($!)
done
started=("" "" "")
deadline=$((t0 + server_start_s * 1000))
while :; do
    waiting=0
    for i in 0 1 2; do
        [ -n "${started[$i]}" ] && continue
        if grep -q '^kbf-server .* reapi=.* worker=' "$SPIKE_TMP/server$i.out"; then
            started[$i]=$(($(now_ms) - t0))
        elif ! kill -0 "${pids[$i]}" 2>/dev/null; then
            rc=0
            wait "${pids[$i]}" || rc=$?
            fail "kbf-server $i exited ($rc) before its start line: $(server_log "$i")"
        else
            waiting=1
        fi
    done
    [ "$waiting" -eq 0 ] && break
    [ "$(now_ms)" -lt "$deadline" ] || fail "a kbf-server printed no start line within ${server_start_s} s"
    sleep 0.02
done
kv build_three_servers_start_ms "${started[*]} (each: launch to start line, all three at once)"

t0=$(now_ms)
kill -TERM "${pids[@]}"
deadline=$((t0 + server_stop_s * 1000))
for i in 0 1 2; do
    # bash reaps an exited child itself (keeping its status for wait), so kill -0
    # fails as soon as the server has exited.
    while kill -0 "${pids[$i]}" 2>/dev/null && [ "$(now_ms)" -lt "$deadline" ]; do
        sleep 0.02
    done
    if kill -0 "${pids[$i]}" 2>/dev/null; then
        fail "kbf-server $i still running ${server_stop_s} s after SIGTERM: $(server_log "$i")"
    fi
    rc=0
    wait "${pids[$i]}" || rc=$?
    [ "$rc" -eq 0 ] || fail "kbf-server $i exited $rc after SIGTERM, not 0: $(server_log "$i")"
done
pids=()
kv build_three_servers_stop_ms "$(($(now_ms) - t0)) (SIGTERM to the last exit)"

# buck2 2026-10-01, sha256 of each release asset as GitHub publishes it.
case $SPIKE_ARCH in
    x86_64) want=828aba01bf80e8ba50ed27d89be86efe0246b3807c3d040e1ea7b4810a55fb78 ;;
    aarch64) want=8df3e94f569b1df74c905e26d18ea6bf6e64f946d0f0cbec90c7398bd33d1861 ;;
esac
url=https://github.com/facebook/buck2/releases/download/2026-10-01/buck2-$SPIKE_ARCH-unknown-linux-gnu.zst
t0=$(now_ms)
curl -fsSL -o "$SPIKE_TMP/buck2.zst" "$url"
kv build_buck2_fetch_ms "$(($(now_ms) - t0))"
echo "$want  $SPIKE_TMP/buck2.zst" | sha256sum -c --quiet
zstd -q -d -f "$SPIKE_TMP/buck2.zst" -o "$SPIKE_TMP/buck2"
chmod +x "$SPIKE_TMP/buck2"
kv build_buck2_version "$("$SPIKE_TMP/buck2" --version 2>&1 | head -n 1)"
kv build_bazel "$(command -v bazel >/dev/null && echo present || echo absent) bazelisk=$(command -v bazelisk >/dev/null && echo present || echo absent)"
