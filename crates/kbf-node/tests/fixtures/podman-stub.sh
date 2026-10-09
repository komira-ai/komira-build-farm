#!/bin/sh
# A `podman` that knows no container: it prints nothing, succeeds, and logs its
# arguments, one per line, to podman.log beside the link it is run through
# (tests/binary.rs links it as `podman` into a directory put first in PATH).
printf '%s\n' "$@" >>"$(dirname "$0")/podman.log"
