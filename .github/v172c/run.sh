#!/bin/bash
# Probe 2 for PR #172 (verifier): the DEVELOPER_TOOLS lock reverted, run as the
# ci step runs it, with the shims test's times printed; then cache-loss hammers.
set -u
P=.github/v172c
T=$(getconf DARWIN_USER_TEMP_DIR)
UF=(cargo test -p kbf-driver-native --test user_folders --locked)
run() {
  label=$1; shift
  "$@" > out.txt 2>&1; rc=$?
  echo "::group::$label"
  grep -E '^test |test result|panicked|TIMES|Running|error' out.txt | head -150
  echo "::endgroup::"
  echo "RESULT $label rc=$rc failed=[$(grep -E '^test .* FAILED$' out.txt | sed 's/^test //; s/ \.\.\. FAILED$//' | tr '\n' ' ')] $(grep TIMES out.txt | tr '\n' ';')"
}
git apply "$P/print-times.patch" && git apply "$P/revert-lock.patch" && git diff --stat
for i in 1 2 3; do run "reverted-ci-step-$i" cargo test -p kbf-outputs -p kbf-driver-native -p kbf-node --locked --no-fail-fast -- --nocapture; done
for i in 1 2 3; do run "reverted-uf-$i" "${UF[@]}" -- --nocapture; done
git apply -R "$P/revert-lock.patch" && git diff --stat
for i in 1 2 3; do run "fix-uf-$i" "${UF[@]}" -- --nocapture; done
run "fix-shims-alone" "${UF[@]}" -- --nocapture --exact the_compiler_shims_print_nothing_on_stderr
( while :; do rm -f "${T}xcrun_db"; sleep 0.02; done ) & h=$!
run "fix-shims-alone-rm-hammer" "${UF[@]}" -- --nocapture --exact the_compiler_shims_print_nothing_on_stderr
kill "$h"; wait "$h" 2> /dev/null
( while :; do xcrun --kill-cache > /dev/null 2>&1; sleep 0.05; done ) & h=$!
run "fix-shims-alone-kill-cache-hammer" "${UF[@]}" -- --nocapture --exact the_compiler_shims_print_nothing_on_stderr
kill "$h"; wait "$h" 2> /dev/null
xcrun --kill-cache; echo "kill-cache rc=$? db: $(ls -l "${T}xcrun_db" 2>&1)"
git status --short
exit 0
