#!/usr/bin/env bash
# Time an integration job pays before its first test: the pinned Rust toolchain, a cold
# `cargo build --workspace --locked` (no cache restored), three kbf-server processes
# started from the result, and a pinned buck2 release fetched and checked by sha256.
# Bazel's presence is recorded (the images ship bazelisk).
#
# Control: the buck2 download must match its pinned sha256, or the step fails.
. "$(dirname "$0")/lib.sh"

t0=$(now_ms)
rustup toolchain install >/dev/null 2>&1
kv build_toolchain_install_ms "$(($(now_ms) - t0))"

t0=$(now_ms)
cargo build --workspace --locked --quiet
kv build_cold_workspace_ms "$(($(now_ms) - t0))"
kv build_target_dir_mib "$(du -sm target | cut -f1)"

t0=$(now_ms)
for i in 1 2 3; do target/debug/kbf-server >/dev/null & done
wait
kv build_three_server_runs_ms "$(($(now_ms) - t0)) (the binary prints its version and exits today)"

# buck2 2026-10-01, sha256 of each release asset as GitHub publishes it.
case $SPIKE_ARCH in
    x86_64) want=828aba01bf80e8ba50ed27d89be86efe0246b3807c3d040e1ea7b4810a55fb78 ;;
    aarch64) want=8df3e94f569b1df74c905e26d18ea6bf6e64f946d0f0cbec90c7398bd33d1861 ;;
esac
url=https://github.com/facebook/buck2/releases/download/2026-10-01/buck2-$SPIKE_ARCH-unknown-linux-gnu.zst
t0=$(now_ms)
curl -fsSL -o "$SPIKE_TMP/buck2.zst" "$url"
kv build_buck2_fetch_ms "$(($(now_ms) - t0))"
echo "$want  $SPIKE_TMP/buck2.zst" | sha256sum -c --quiet
zstd -q -d -f "$SPIKE_TMP/buck2.zst" -o "$SPIKE_TMP/buck2"
chmod +x "$SPIKE_TMP/buck2"
kv build_buck2_version "$("$SPIKE_TMP/buck2" --version 2>&1 | head -n 1)"
kv build_bazel "$(command -v bazel >/dev/null && echo present || echo absent) bazelisk=$(command -v bazelisk >/dev/null && echo present || echo absent)"
