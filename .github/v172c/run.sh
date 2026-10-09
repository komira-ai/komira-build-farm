#!/bin/bash
# Probe 3 for PR #172 (verifier): the shims test alone, with and without other
# developer tools (swiftc, swift build) running beside it, as the other tests do.
set -u
P=.github/v172c
UF=(cargo test -p kbf-driver-native --test user_folders --locked)
W="$RUNNER_TEMP/v172c"; mkdir -p "$W/pkg/Sources/hello"
echo 'print("hi")' > "$W/a.swift"; echo 'print("hi")' > "$W/pkg/Sources/hello/main.swift"
printf '// swift-tools-version:5.9\nimport PackageDescription\nlet package = Package(name: "hello", targets: [.executableTarget(name: "hello")])\n' > "$W/pkg/Package.swift"
run() {
  label=$1; shift
  "$@" > out.txt 2>&1; rc=$?
  echo "::group::$label"
  grep -E '^test |test result|panicked|TIMES|error' out.txt | head -40
  echo "::endgroup::"
  echo "RESULT $label rc=$rc failed=[$(grep -E '^test .* FAILED$' out.txt | sed 's/^test //; s/ \.\.\. FAILED$//' | tr '\n' ' ')] $(grep TIMES out.txt | tr '\n' ';')"
}
git apply "$P/print-times.patch" && git diff --stat
"${UF[@]}" --no-run > /dev/null 2>&1
S=(--nocapture --exact the_compiler_shims_print_nothing_on_stderr)
for i in 1 2 3; do
  run "alone-$i" "${UF[@]}" -- "${S[@]}"
  ( cd "$W" && while :; do xcrun swiftc -o a.out a.swift > /dev/null 2>&1; done ) & h=$!
  run "beside-swiftc-$i" "${UF[@]}" -- "${S[@]}"
  kill "$h"; wait "$h" 2> /dev/null
  ( cd "$W/pkg" && while :; do rm -rf .build; swift build > /dev/null 2>&1; done ) & h=$!
  run "beside-swift-build-$i" "${UF[@]}" -- "${S[@]}"
  kill "$h"; wait "$h" 2> /dev/null; pkill -f swift-build; sleep 1
done
git status --short
exit 0
