#!/usr/bin/env bash
# The integration test's footprint on one hosted runner: three server stand-ins, MinIO
# and RustFS, and two "daemon" containers, all at once. The kbf binaries are not
# servers yet, so the stand-ins are small HTTP servers on loopback: this measures the
# budget (start-to-ready time, memory left, ports), not kbf's behaviour.
#
# Control: every one of the seven endpoints must answer, or the step fails.
# Needs `spike-minio` from podman_rootless.sh (on port 19000).
. "$(dirname "$0")/lib.sh"
user_session

avail() { awk '/^MemAvailable/ {print int($2/1024)}' /proc/meminfo; }
a0=$(avail)
t0=$(now_ms)

pids=()
for p in 19301 19302 19303; do
    python3 -m http.server --bind 127.0.0.1 "$p" --directory "$SPIKE_TMP" >/dev/null 2>&1 &
    pids+=("$!")
done
podman rm -f spike-rustfs spike-daemon-1 spike-daemon-2 >/dev/null 2>&1 || true
podman run -d --name spike-rustfs -p 127.0.0.1:19100:9000 \
    -e RUSTFS_ACCESS_KEY=kbf-ci-access -e RUSTFS_SECRET_KEY=kbf-ci-throwaway "$SPIKE_RUSTFS" >/dev/null
for i in 1 2; do
    podman run -d --name "spike-daemon-$i" -p "127.0.0.1:1920$i:8080" "$SPIKE_BUSYBOX" \
        httpd -f -p 8080 -h /etc >/dev/null
done

ok=0
for p in 19000 19100 19201 19202 19301 19302 19303; do
    if wait_http "http://127.0.0.1:$p/" 60; then ok=$((ok + 1)); else kv "servers_port_${p}" "no answer"; fi
done
kv servers_ready_ms "$(($(now_ms) - t0))"
kv servers_answering "$ok/7"
kv servers_mem_available_before_mib "$a0"
kv servers_mem_available_after_mib "$(avail)"
kv servers_container_mem "$(podman stats --no-stream --format '{{.Name}}={{.MemUsage}}' | tr '\n' ' ' | tr -s ' ')"
rss=0
for p in "${pids[@]}"; do rss=$((rss + $(awk '/^VmRSS/ {print $2}' "/proc/$p/status"))); done
kv servers_standin_rss_kib_total "$rss"
kv servers_listening_tcp_sockets "$(ss -Htln | wc -l)"
kv servers_load_avg "$(cut -d' ' -f1-3 /proc/loadavg)"

kill "${pids[@]}" 2>/dev/null || true
podman rm -f spike-rustfs spike-daemon-1 spike-daemon-2 spike-minio >/dev/null 2>&1 || true
[ "$ok" -eq 7 ] || { echo "only $ok of 7 endpoints answered" >&2; exit 1; }
