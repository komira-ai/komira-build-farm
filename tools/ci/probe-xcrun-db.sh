#!/usr/bin/env bash
# PROBE, do not merge: what is in the user's xcrun cache at each step of native-macos.
#   snap <label>   stat the cache and count its entries per Xcode, without running xcrun
#   time <label>   time `xcrun --find clang` under each Xcode's DEVELOPER_DIR
#   fill <label>   look up many tools under one Xcode, as a build does
set -u
T=$(getconf DARWIN_USER_TEMP_DIR)
DB="${T%/}/xcrun_db"
ms() { perl -MTime::HiRes=time -e 'printf "%.0f", time*1000'; }
snap() {
  echo "== xcrun-probe snap $1 at $(date -u +%H:%M:%S)"
  if [ -e "$DB" ]; then
    ls -laT "$DB"
    echo "md5 $(md5 -q "$DB")"
    echo "entries naming each app:"
    strings -n 6 "$DB" | grep -o '/Applications/Xcode[^/]*\.app' | sort | uniq -c | sed 's/^/  /'
    echo "clang mentions: $(strings -n 4 "$DB" | grep -c 'clang')  total strings: $(strings -n 6 "$DB" | wc -l)"
  else
    echo "no $DB"
  fi
  ls -la "$T" | grep -i xcrun || true
}
timed() {
  echo "== xcrun-probe time $1"
  for app in /Applications/Xcode*.app; do
    s=$(ms); DEVELOPER_DIR="$app/Contents/Developer" /usr/bin/xcrun --find clang >/dev/null 2>&1; rc=$?; e=$(ms)
    echo "  $((e - s)) ms rc=$rc $app"
  done
}
fill() {
  echo "== xcrun-probe fill $1"
  app=$(ls -d /Applications/Xcode*.app | tail -1)
  s=$(ms)
  for t in clang clang++ swift swiftc ld ar ranlib nm strip lipo otool libtool dsymutil lldb \
           actool ibtool momc mapc xcodebuild xctest swift-frontend codesign_allocate \
           install_name_tool size strings segedit metal metallib clang-format swift-format \
           git make cmake ninja python3 swift-build swift-package swift-test docc \
           xcstringstool assetutil plutil ld-classic dwarfdump llvm-cov llvm-profdata \
           sourcekit-lsp swift-demangle swift-symbolgraph-extract unifdef yacc lex bison flex \
           gperf m4 indent ctags; do
    DEVELOPER_DIR="$app/Contents/Developer" /usr/bin/xcrun --find "$t" >/dev/null 2>&1
    DEVELOPER_DIR="$app/Contents/Developer" /usr/bin/xcrun --sdk macosx --find "$t" >/dev/null 2>&1
  done
  echo "  filled under $app in $(( $(ms) - s )) ms"
}
case "$1" in
  snap) snap "$2" ;;
  time) timed "$2" ;;
  fill) fill "$2" ;;
esac
