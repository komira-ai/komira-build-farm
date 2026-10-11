#!/bin/sh
# A `podman` that knows no container: it prints nothing, succeeds, and logs its
# arguments, one per line, to podman.log beside the link it is run through
# (tests/binary.rs links it as `podman` into a directory put first in PATH).
# It knows an image only while `image-id` beside the link exists: `image inspect`
# prints that id, whatever image it is asked about, and fails as Podman does for an
# image not in the store when the file is absent. `info` names `store/` beside the
# link as the directory of the image store's per-image directories.
here=$(dirname "$0")
printf '%s\n' "$@" >>"$here/podman.log"
verb=
for arg; do
    case $arg in
    -*) ;;
    *) verb=$arg; break ;;
    esac
done
case $verb in
image)
    [ -f "$here/image-id" ] || { echo "Error: $here: image not known" >&2; exit 125; }
    cat "$here/image-id"
    ;;
info) echo "$here/store" ;;
esac
exit 0
