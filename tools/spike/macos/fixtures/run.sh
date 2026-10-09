#!/usr/bin/env bash
# Builds one fixture and runs its UI tests, then reports what the .xcresult holds.
# macOS only. Usage: run.sh ios|macos OUT_DIR
#
# ios runs on a simulator: the first available iPhone of the newest iOS runtime.
# macos runs in this login session. Prints `SPIKE macos-<arch>.xcuitest.<platform>.*`
# lines (lib.sh): xcodebuild's exit status, each test's result, each KBF-FIXTURE line
# the tests printed, and each attachment exported from the bundle with the test that
# made it. Exits 0 only if xcodebuild exited 0 and exactly the three expected tests
# passed, so a run that tested nothing is a failure.
. "$(dirname "$0")/../../lib.sh"
SPIKE_ARCH="macos-$(uname -m)"

here=$(cd "$(dirname "$0")" && pwd)
platform=${1:?usage: run.sh ios|macos OUT_DIR}
out=${2:?usage: run.sh ios|macos OUT_DIR}
case $platform in
ios)
    project=IOSFixture
    udid=$(xcrun simctl list devices available -j | python3 -I -c '
import json, sys
d = json.load(sys.stdin)["devices"]
ios = sorted((k for k in d if ".iOS-" in k), key=lambda k: [int(p) for p in k.rsplit(".iOS-", 1)[1].split("-")])
for k in reversed(ios):
    phones = [x for x in d[k] if x["name"].startswith("iPhone")]
    if phones:
        print(phones[0]["udid"], k.rsplit(".", 1)[1], phones[0]["name"].replace(" ", "_"))
        break
')
    read -r udid runtime device <<<"$udid" || true
    if [ -z "${udid:-}" ]; then
        kv "xcuitest.ios.simulator" "none available"
        exit 1
    fi
    kv "xcuitest.ios.simulator" "$device $runtime"
    destination="platform=iOS Simulator,id=$udid"
    ;;
macos)
    project=MacFixture
    destination="platform=macOS,arch=$(uname -m)"
    ;;
*)
    echo "usage: run.sh ios|macos OUT_DIR" >&2
    exit 2
    ;;
esac

mkdir -p "$out"
bundle=$out/$platform.xcresult
log=$out/$platform-xcodebuild.log
rm -rf "$bundle"
start=$(date +%s)
rc=0
xcodebuild test -project "$here/$project.xcodeproj" -scheme "$project" \
    -destination "$destination" -derivedDataPath "$out/$platform-derived" \
    -resultBundlePath "$bundle" >"$log" 2>&1 || rc=$?
kv "xcuitest.$platform.xcodebuild_exit" "$rc"
kv "xcuitest.$platform.seconds" "$(($(date +%s) - start))"
if [ "$rc" -ne 0 ]; then
    grep -E 'error:|failed|FAIL' "$log" | head -n 20 || true
fi
{ grep -o 'KBF-FIXTURE [a-z-]*=.*' "$log" || true; } | sort -u | while read -r _ line; do
    kv "xcuitest.$platform.${line%%=*}" "${line#*=}"
done

if [ ! -d "$bundle" ]; then
    kv "xcuitest.$platform.xcresult" "absent"
    exit 1
fi
passed=$(xcrun xcresulttool get test-results tests --path "$bundle" | python3 -I -c '
import json, sys
def walk(node, out):
    if node.get("nodeType") == "Test Case":
        out.append((node["name"], node.get("result", "?")))
    for c in node.get("children", []):
        walk(c, out)
cases = []
for n in json.load(sys.stdin).get("testNodes", []):
    walk(n, cases)
want = {"testTapChangesLabel()", "testScreenshotShowsPattern()", "testPatternLeftOutIsNotFound()"}
for name, result in cases:
    print("case", name, result)
print("passed", int(len(cases) == 3 and {n for n, r in cases if r == "Passed"} == want))
')
while read -r kind name result; do
    if [ "$kind" = case ]; then kv "xcuitest.$platform.test.$name" "$result"; fi
done <<<"$passed"
all_passed=$(sed -n 's/^passed //p' <<<"$passed")

# Attachments: exported with the manifest that names the test and attachment each
# file came from. Inside the bundle they are content-addressed files under Data/.
attachments=$out/$platform-attachments
rm -rf "$attachments"
if xcrun xcresulttool export attachments --path "$bundle" --output-path "$attachments" >/dev/null 2>&1; then
    python3 -I - "$attachments/manifest.json" <<'PY' | while read -r test name file; do
import json, sys
for t in json.load(open(sys.argv[1])):
    for a in t.get("attachments", []):
        print(t.get("testIdentifier", "?"), a.get("suggestedHumanReadableName", "?").replace(" ", "_"), a.get("exportedFileName", "?"))
PY
        kv "xcuitest.$platform.attachment" "$test $name -> $file"
    done
    kv "xcuitest.$platform.bundle_data_files" "$(find "$bundle/Data" -type f | wc -l | tr -d ' ')"
else
    kv "xcuitest.$platform.attachment" "xcresulttool export attachments failed"
fi

kv "xcuitest.$platform.result" "$([ "$rc" -eq 0 ] && [ "$all_passed" = 1 ] && echo PASS || echo FAIL)"
[ "$rc" -eq 0 ] && [ "$all_passed" = 1 ]
