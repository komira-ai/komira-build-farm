#!/usr/bin/env bash
# A stand-in for `podman`. Each test links `<dir>/podman` to this file; the fake keeps
# its state and the test's knobs in `<dir>/state` and uses `<dir>/cgroup` as the cgroup
# mount. It runs the action as a plain process, so the driver's steps can be tested on
# any machine. Real Podman behaviour is tested in tests/podman.rs.
#
# Knobs (files in $STATE the test writes):
#   image-id         what `image inspect` prints; absent: the image is not in the store
#   image-hangs      `image inspect` hangs (a slow prepare step)
#   info-fails       `info` fails
#   store/           the image store `info` names (store/fake-images/<id>/=<key>)
#   action.sh        what `start --attach` runs (env: ROOT, UPPER, CG)
#   create-fails     `create` fails
#   block-log        `create` makes a directory where the log file this knob names
#                    (stdout or stderr) goes, so the driver cannot create it
#   vanish-after-create  `create` unlinks the podman program it was run as
#   status           written by `start`; a test may preset `status-override`
#   inspect-fails    `inspect` fails
#   kill-fails       `kill` fails
#   rm-fails         `rm` fails
#   unshare-noop     `unshare rm` succeeds without removing anything
#   unshare-fails    `unshare rm` fails
#   chown-fails      `unshare chown` to the owner this knob holds (`1:1` or `0:0`) fails
# Records: create.args (one argument per line), calls (one verb per line; `unshare`
#   with its command, `unshare rm` or `unshare chown`), removed,
#   killed-before-rm (`rm` found the lease cgroup's cgroup.kill already written),
#   events (in order: `cgroup.kill ended the action`, `start ended` once `start` has
#   waited for the action and written its status, `rm`, `chown <owner> <paths...>`).
#   The fake changes no owner (the test runs as one user), but models what an owner
#   other than the daemon's means: when `start` ends while the overlay is the
#   container's (`chown 1:1` last, recorded in `owner`), it makes the upper directory
#   unreadable (mode 000, the old mode kept in `upper-mode`), as files the action made
#   private would be to the daemon's user; `chown 0:0` restores the mode. So outputs
#   read before the overlay is handed back fail as they would on real Podman.
set -u
here=$(dirname "$0")
STATE=$here/state
CGROOT=$here/cgroup

[ "${1:-}" = --cgroup-manager=cgroupfs ] || { echo "fake podman: missing --cgroup-manager" >&2; exit 125; }
shift
verb=$1
shift
if [ "$verb" = unshare ]; then
    echo "unshare ${1:-}" >>"$STATE/calls"
else
    echo "$verb" >>"$STATE/calls"
fi

case $verb in
image)
    [ -f "$STATE/image-hangs" ] && exec sleep 30
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
    [ -f "$STATE/block-log" ] && mkdir "$(dirname "$(cat "$STATE/root")")/$(cat "$STATE/block-log")"
    # The podman program disappears once the container exists.
    [ -f "$STATE/vanish-after-create" ] && unlink "$0"
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
        if [ -e "$cg/cgroup.kill" ]; then
            # Recorded before the kill, so it precedes `start ended`.
            echo "cgroup.kill ended the action" >>"$STATE/events"
            kill -KILL "$pid" 2>/dev/null
            break
        fi
        sleep 0.02
    done) &
    wait "$pid"
    code=$?
    if [ -f "$STATE/status-override" ]; then
        cp "$STATE/status-override" "$STATE/status"
    else
        echo "exited $code" >"$STATE/status"
    fi
    echo "start ended" >>"$STATE/events"
    if [ "$(cat "$STATE/owner" 2>/dev/null)" = 1:1 ]; then
        upper=$(cat "$STATE/upper")
        stat -c %a "$upper" >"$STATE/upper-mode"
        chmod 000 "$upper"
    fi
    exit "$code"
    ;;
inspect)
    [ -f "$STATE/inspect-fails" ] && { echo "Error: inspect refused" >&2; exit 125; }
    [ -f "$STATE/status" ] || { echo "Error: no such container" >&2; exit 125; }
    cat "$STATE/status"
    ;;
kill)
    [ -f "$STATE/kill-fails" ] && { echo "Error: kill refused" >&2; exit 125; }
    [ -f "$STATE/pid" ] || { echo "Error: no such container" >&2; exit 125; }
    kill "-${1#--signal=}" "$(cat "$STATE/pid")"
    ;;
rm)
    [ -f "$STATE/rm-fails" ] && { echo "Error: rm refused" >&2; exit 125; }
    touch "$STATE/removed"
    echo rm >>"$STATE/events"
    if [ -f "$STATE/cgroup" ] && [ -e "$CGROOT$(cat "$STATE/cgroup")/cgroup.kill" ]; then
        touch "$STATE/killed-before-rm"
    fi
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
    if [ "$1" = chown ]; then
        # chown -hR OWNER -- PATHS...
        [ "$2" = -hR ] && [ "$4" = -- ] || { echo "fake podman: chown $*" >&2; exit 125; }
        owner=$3
        shift 4
        echo "chown $owner $*" >>"$STATE/events"
        if [ -f "$STATE/chown-fails" ] && [ "$(cat "$STATE/chown-fails")" = "$owner" ]; then
            echo "Error: chown refused" >&2
            exit 1
        fi
        echo "$owner" >"$STATE/owner"
        if [ "$owner" = 0:0 ] && [ -f "$STATE/upper-mode" ]; then
            chmod "$(cat "$STATE/upper-mode")" "$(cat "$STATE/upper")"
            rm -f -- "$STATE/upper-mode"
        fi
        exit 0
    fi
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
