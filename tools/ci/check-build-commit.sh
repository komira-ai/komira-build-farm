#!/usr/bin/env bash
# Checks that kbf-server names the commit it was built from (crates/kbf-server/build.rs).
#
#   check-build-commit.sh check <binary> <commit>
#     `<binary> --version` prints exactly `<name> <package version>+<first 12 of
#     commit>`, where <name> is the binary's file name. Control: the same output does
#     not pass for a commit with its first digit changed, so a comparison that accepts
#     anything fails the job instead of passing it.
#
#   check-build-commit.sh stamps <commit>
#     From the checkout: a plain `cargo build` of kbf-server passes `check` for
#     <commit>; two builds with KBF_BUILD_COMMIT_OVERRIDE set to two stamps each pass
#     for their own stamp and not for <commit>; and a build with an override that is
#     not 12 lowercase hex digits fails. Leaves the last good stamp built in target/.
set -euo pipefail

usage() { echo "usage: $0 check <binary> <commit> | stamps <commit>" >&2; exit 2; }

# Whether `<binary> --version` names `commit` (the first 12 characters are used).
names() {
  local bin=$1 short=${2:0:12} out
  out=$("$bin" --version)
  [[ $out =~ ^$(basename "$bin")\ [0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?\+${short}$ ]]
}

check() {
  local bin=$1 commit=$2
  [[ $commit =~ ^[0-9a-f]{12,40}$ ]] || { echo "not a commit: '$commit'" >&2; exit 1; }
  if ! names "$bin" "$commit"; then
    echo "$bin --version is '$("$bin" --version)'; expected it to end +${commit:0:12}" >&2
    exit 1
  fi
  local other
  if [ "${commit:0:1}" = 0 ]; then other=1${commit:1}; else other=0${commit:1}; fi
  if names "$bin" "$other"; then
    echo "control: '$("$bin" --version)' also passed for ${other:0:12}" >&2
    exit 1
  fi
  echo "ok: $("$bin" --version)"
}

build() { cargo build --locked -p kbf-server --bin kbf-server; }

stamps() {
  local commit=$1 bin=target/debug/kbf-server stamp
  (unset KBF_BUILD_COMMIT_OVERRIDE; build)
  check "$bin" "$commit"
  for stamp in a1a1a1a1a1a1 b2b2b2b2b2b2; do
    KBF_BUILD_COMMIT_OVERRIDE=$stamp build
    check "$bin" "$stamp"
    if names "$bin" "$commit"; then
      echo "the build stamped $stamp still names $commit" >&2
      exit 1
    fi
  done
  local log
  if log=$(KBF_BUILD_COMMIT_OVERRIDE=not-a-commit build 2>&1); then
    echo "a build with KBF_BUILD_COMMIT_OVERRIDE=not-a-commit succeeded" >&2
    exit 1
  fi
  if ! grep -q 'KBF_BUILD_COMMIT_OVERRIDE=.*expected exactly 12 lowercase hex digits' <<<"$log"; then
    printf 'the build failed, but not on the override:\n%s\n' "$log" >&2
    exit 1
  fi
  echo "ok: a malformed override fails the build"
}

case "${1:-}" in
  check) [ "$#" -eq 3 ] || usage; check "$2" "$3" ;;
  stamps) [ "$#" -eq 2 ] || usage; stamps "$2" ;;
  *) usage ;;
esac
