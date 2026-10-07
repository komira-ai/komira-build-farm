#!/usr/bin/env bash
# A stand-in for `podman`. Each test links `<dir>/podman` to this file; the fake keeps
# its state and the test's knobs in `<dir>/state` and uses `<dir>/cgroup` as the cgroup
# mount. It runs the action as a plain process, so the driver's steps can be tested on
# any machine. Real Podman behaviour is tested in tests/podman.rs.
#
# Knobs (files in $STATE the test writes):
#   image-id         what `image inspect` prints; absent: the image is not in the store
#   info-fails       `info` fails
#   store/           the image store `info` names (store/fake-images/<id>/=<key>)
#   action.sh        what `start --attach` runs (env: ROOT, UPPER, CG)
#   create-fails     `create` fails
#   status           written by `start`; a test may preset `status-override`
#   rm-fails         `rm` fails
#   unshare-noop     `unshare rm` succeeds without removing anything
#   unshare-fails    `unshare rm` fails
# Records: create.args (one argument per line), calls (one verb per line), removed.
set -u
here=$(dirname "$0")
STATE=$here/state
CGROOT=$here/cgroup

[ "${1:-}" = --cgroup-manager=cgroupfs ] || { echo "fake podman: missing --cgroup-manager" >&2; exit 125; }
shift
verb=$1
shift
echo "$verb" >>"$STATE/calls"

case $verb in
image)
    [ -f "$STATE/image-id" ] || { echo "Error: image not known" >&2; exit 125; }
    cat "$STATE/image-id"
    ;;
info)
    [ -f "$STATE/info-fails" ] && { echo "Error: info refused" >&2; exit 125; }
    echo "$STATE/store/fake-images"
    ;;
create)
    printf '%s\n' "$@" >"$STATE/create.args"
    for arg in "$@"; do
        case $arg in
        --cgroup-parent=*) echo "${arg#--cgroup-parent=}" >"$STATE/cgroup" ;;
        --volume=*)
            v=${arg#--volume=}
            echo "${v%%:*}" >"$STATE/root"
            u=${v#*upperdir=}
            echo "${u%%,*}" >"$STATE/upper"
            ;;
        esac
    done
    [ -f "$STATE/create-fails" ] && { echo "Error: create refused" >&2; exit 125; }
    echo 0123456789ab
    ;;
start)
    CG="$CGROOT$(cat "$STATE/cgroup")" ROOT=$(cat "$STATE/root") UPPER=$(cat "$STATE/upper") \
        bash "$STATE/action.sh" &
    pid=$!
    echo "$pid" >"$STATE/pid"
    cg="$CGROOT$(cat "$STATE/cgroup")"
    # cgroup.kill: a write to the file kills the action, as the kernel would.
    (while kill -0 "$pid" 2>/dev/null; do
        [ -e "$cg/cgroup.kill" ] && kill -KILL "$pid" 2>/dev/null
        sleep 0.02
    done) &
    wait "$pid"
    code=$?
    if [ -f "$STATE/status-override" ]; then
        cp "$STATE/status-override" "$STATE/status"
    else
        echo "exited $code" >"$STATE/status"
    fi
    exit "$code"
    ;;
inspect)
    [ -f "$STATE/status" ] || { echo "Error: no such container" >&2; exit 125; }
    cat "$STATE/status"
    ;;
kill)
    [ -f "$STATE/pid" ] || { echo "Error: no such container" >&2; exit 125; }
    kill "-${1#--signal=}" "$(cat "$STATE/pid")"
    ;;
rm)
    [ -f "$STATE/rm-fails" ] && { echo "Error: rm refused" >&2; exit 125; }
    touch "$STATE/removed"
    # --force: a container still running is killed.
    [ -f "$STATE/pid" ] && kill -KILL "$(cat "$STATE/pid")" 2>/dev/null
    # Interface files are not files to rmdir on cgroupfs; here they are, so the fake
    # removes them the way the kernel would make them vanish.
    if [ -f "$STATE/cgroup" ]; then
        find "$CGROOT$(cat "$STATE/cgroup")" -type f -delete 2>/dev/null
    fi
    exit 0
    ;;
unshare)
    [ -f "$STATE/unshare-fails" ] && { echo "Error: unshare refused" >&2; exit 1; }
    [ -f "$STATE/unshare-noop" ] && exit 0
    # rm -rf -- DIR
    dir=${4:?}
    chmod -R u+rwx "$dir"
    rm -rf -- "$dir"
    ;;
*)
    echo "fake podman: unexpected verb $verb" >&2
    exit 125
    ;;
esac
