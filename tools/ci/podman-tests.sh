#!/usr/bin/env bash
# PROBE: user namespace modes for rootless Podman. Not for merge.
set -uo pipefail
BUSYBOX=docker.io/library/busybox@sha256:bdf57e528e45e4433820e045b29b4597825a1c9e38353532d90a01445013f82e
uid=$(id -u)
if [ ! -S "/run/user/$uid/bus" ]; then
    sudo loginctl enable-linger "$(id -un)"
    for _ in $(seq 100); do [ -S "/run/user/$uid/bus" ] && break; sleep 0.1; done
fi
export XDG_RUNTIME_DIR=/run/user/$uid
x() { echo "+ $*"; "$@"; echo "= exit $?"; }
x id; x uname -r; x podman --version
x grep "^$(id -un):" /etc/subuid /etc/subgid
x podman info --format '{{.Store.GraphDriverName}} {{.Host.OCIRuntime.Name}} {{json .Host.IDMappings}} {{json .Store.GraphOptions}}'
x podman unshare cat /proc/self/uid_map
podman pull -q "$BUSYBOX" >/dev/null
x podman run --rm --userns=auto "$BUSYBOX" sh -c 'cat /proc/self/uid_map /proc/self/gid_map; id'
x podman run --rm --userns=nomap "$BUSYBOX" sh -c 'cat /proc/self/uid_map /proc/self/gid_map; id'
x podman run --rm "$BUSYBOX" sh -c 'cat /proc/self/uid_map; id'
x podman create --userns=auto --name t-auto "$BUSYBOX" true
x podman inspect --format '{{json .HostConfig.IDMappings}}' t-auto
x podman create --userns=auto --name t-auto2 "$BUSYBOX" true
x podman inspect --format '{{json .HostConfig.IDMappings}}' t-auto2
x podman create --userns=nomap --name t-nomap "$BUSYBOX" true
x podman inspect --format '{{json .HostConfig.IDMappings}}' t-nomap
podman rm -f t-auto t-auto2 t-nomap >/dev/null
W=$HOME/probe
SCRIPT='id; ls -ln /kbf/root /kbf/root/out; echo x > out/a; echo ax=$?; mkdir out/p; echo s > out/p/s; chmod 600 out/p/s; chmod 700 out/p; echo y > in/g; echo iny=$?; echo z > in/f; echo inz=$?; echo top > t; echo top=$?'
fresh() {
    [ -d "$W" ] && podman unshare rm -rf "$W"
    mkdir -p "$W/root/in" "$W/upper/out" "$W/work"; echo hi > "$W/root/in/f"
}
after() {
    echo "--- host view"; ls -lnR "$W/upper"; cat "$W/upper/out/p/s"; echo "cat-secret=$?"
    ls "$W/upper/out/p"; echo "ls-private=$?"
}
for mode in default auto nomap; do
    echo "===== overlay, no chown, $mode"
    fresh
    flag=(); [ $mode != default ] && flag=(--userns=$mode)
    x podman run --rm "${flag[@]}" -v "$W/root:/kbf/root:O,upperdir=$W/upper,workdir=$W/work" --workdir /kbf/root "$BUSYBOX" sh -c "$SCRIPT"
    after
done
echo "===== nomap, pre-chown 1:1 then chown back 0:0"
fresh
x podman unshare chown -R 1:1 "$W/root" "$W/upper" "$W/work"
x podman run --rm --userns=nomap -v "$W/root:/kbf/root:O,upperdir=$W/upper,workdir=$W/work" --workdir /kbf/root "$BUSYBOX" sh -c "$SCRIPT; adduser -D -u 1000 u; su u -c 'echo u > /kbf/root/out/byu; id' ; ls -ln /kbf/root/out"
after
x podman unshare chown -hR 0:0 "$W/upper"
after
echo "===== auto, inspect before start"
fresh
x podman create --userns=auto --name t-a -v "$W/root:/kbf/root:O,upperdir=$W/upper,workdir=$W/work" --workdir /kbf/root "$BUSYBOX" sh -c "$SCRIPT"
x podman inspect --format '{{json .HostConfig.IDMappings}}' t-a
podman rm -f t-a >/dev/null
echo "===== auto, :O,U"
fresh
x podman run --rm --userns=auto -v "$W/root:/kbf/root:O,U,upperdir=$W/upper,workdir=$W/work" --workdir /kbf/root "$BUSYBOX" sh -c "$SCRIPT"
after; ls -lnd "$W/root" "$W/root/in" "$W/root/in/f" "$W/upper" "$W/work"
echo "===== nomap, :O,U"
fresh
x podman run --rm --userns=nomap -v "$W/root:/kbf/root:O,U,upperdir=$W/upper,workdir=$W/work" --workdir /kbf/root "$BUSYBOX" sh -c "$SCRIPT"
after; ls -lnd "$W/root" "$W/root/in" "$W/root/in/f" "$W/upper" "$W/work"
echo "===== many auto containers at once"
for i in $(seq 1 80); do
    podman create --userns=auto --name "m$i" "$BUSYBOX" true >/dev/null 2>"$W.err" || { echo "auto create $i failed: $(cat "$W.err")"; break; }
done
podman inspect --format '{{.Name}} {{json .HostConfig.IDMappings.UIDMap}}' m1 m2 m3
podman rm -af >/dev/null
echo "===== nomap timing: run 5 containers"
time (for i in 1 2 3 4 5; do podman run --rm --userns=nomap "$BUSYBOX" true; done)
time (for i in 1 2 3 4 5; do podman run --rm --userns=auto "$BUSYBOX" true; done)
time (for i in 1 2 3 4 5; do podman run --rm "$BUSYBOX" true; done)
exit 1
