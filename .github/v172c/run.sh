#!/bin/bash
# Probe for PR #172 (verifier): the fix reverted, the fix kept, a cache-loss
# hammer, and the sandbox mutants, each read from the RESULT lines.
set -u
P=.github/v172c
UF=(cargo test -p kbf-driver-native --test user_folders --locked)
LIB=(cargo test -p kbf-driver-native --lib --locked user_folders)
run() {
  label=$1; shift
  "$@" > out.txt 2>&1; rc=$?
  echo "::group::$label"
  grep -E '^test |test result|panicked|cc [0-9]|xcode=|error' out.txt | head -80
  echo "::endgroup::"
  echo "RESULT $label rc=$rc failed=[$(grep -E '^test .* FAILED$' out.txt | sed 's/^test //; s/ \.\.\. FAILED$//' | tr '\n' ' ')]"
}
"${UF[@]}" --no-run > /dev/null 2>&1; "${LIB[@]}" --no-run > /dev/null 2>&1
git apply "$P/revert-lock.patch" && git diff --stat
"${UF[@]}" --no-run > /dev/null 2>&1
for i in 1 2 3 4 5; do run "reverted-$i" "${UF[@]}"; done
git apply -R "$P/revert-lock.patch" && git diff --stat
"${UF[@]}" --no-run > /dev/null 2>&1
for i in 1 2 3; do run "fix-$i" "${UF[@]}"; done
run "fix-shims-alone" "${UF[@]}" -- --exact the_compiler_shims_print_nothing_on_stderr
( while :; do xcrun --kill-cache > /dev/null 2>&1; sleep 0.05; done ) & h=$!
run "fix-shims-alone-cache-hammer" "${UF[@]}" -- --exact the_compiler_shims_print_nothing_on_stderr
kill "$h"; wait "$h" 2> /dev/null
for m in no-xcrun-rule no-trailing-slash no-owner-check follow-symlinks; do
  git apply "$P/$m.patch" && git diff --stat
  run "mut-$m-lib" "${LIB[@]}"
  run "mut-$m-uf" "${UF[@]}"
  git apply -R "$P/$m.patch"
done
git status --short
exit 0
