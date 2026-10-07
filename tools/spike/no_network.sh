#!/usr/bin/env bash
# No network for an action: a rootless container with --network=none must fail a DNS
# lookup and a connect to the MinIO port on the host's own address.
#
# Arms, all with the same busybox commands:
# - none: the arm under test; both checks must fail;
# - default network: the control showing the checks can succeed from a container;
# - host network: the planted defect (an action given the host network); the MinIO
#   check must succeed, or a test built this way could never go red.
# The step fails if the host-network arm cannot reach MinIO (the check would be vacuous).
# Needs `spike-minio` from podman_rootless.sh. Prints outcomes, never addresses.
. "$(dirname "$0")/lib.sh"
user_session

host_ip=$(hostname -I | awk '{print $1}')

# probe NETWORK TARGET-HOST: prints "dns=<ok|fail> minio=<ok|fail>".
probe() {
    podman run --rm --network "$1" -e T="$2" "$SPIKE_BUSYBOX" sh -c '
        if timeout 5 nslookup example.com >/dev/null 2>&1; then d=ok; else d=fail; fi
        if wget -q -T 3 -O /dev/null "http://$T:19000/minio/health/live" 2>/dev/null; then m=ok; else m=fail; fi
        printf "dns=%s minio=%s" "$d" "$m"' 2>&1 | tail -n 1 || true
}

none=$(probe none "$host_ip")
default=$(probe "${SPIKE_DEFAULT_NET:-slirp4netns}" "$host_ip")
default_named=$(probe "${SPIKE_DEFAULT_NET:-slirp4netns}" host.containers.internal)
pasta=$(probe pasta "$host_ip")
host=$(probe host 127.0.0.1)
kv netnone_none "$none"
kv netnone_slirp4netns_host_addr "$default"
kv netnone_slirp4netns_host_name "$default_named"
kv netnone_pasta_host_addr "$pasta"
kv netnone_host_mutant "$host"

case $none in
    "dns=fail minio=fail") verdict=isolated ;;
    *) verdict="LEAK ($none)" ;;
esac
kv netnone_verdict "$verdict"
case $host in
    *minio=ok*) ;;
    *) echo "control failed: the host-network arm did not reach MinIO ($host)" >&2; exit 1 ;;
esac
