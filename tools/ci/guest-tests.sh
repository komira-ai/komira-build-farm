#!/usr/bin/env bash
# kbf-guest as launchd and sudo start it, on a hosted runner (crates/kbf-guest). The
# cargo tests run the protocol and the commands; this checks what they cannot:
#
# - on every runner: started as root through sudo, every subcommand refuses and exits 2;
# - on macOS: started by launchd as a LaunchAgent in the runner's GUI session,
#   `kbf-guest session` reports Aqua; started as a LaunchDaemon in the system domain
#   (as the runner's user, since it refuses root) it reports System. The daemon is the
#   control: an agent that always said Aqua would pass the first check alone.
#
# GitHub's hosted runners allow passwordless sudo. Where it is not allowed this script
# says so and checks only what needs no root.
set -euo pipefail

fail() { echo "FAIL: $*" >&2; exit 1; }
step() { echo; echo "--- $*"; }

cargo build --locked -p kbf-guest --bin kbf-guest
tmp=$(mktemp -d "${RUNNER_TEMP:-/tmp}/kbf-guest-ci.XXXXXX")
chmod 0755 "$tmp"
bin="$tmp/kbf-guest"
cp target/debug/kbf-guest "$bin"
chmod 0755 "$bin"
sudo_ok=false
if sudo -n true 2>/dev/null; then sudo_ok=true; fi

step "as root, every subcommand refuses"
if $sudo_ok; then
  for args in "session" "serve --unix-socket $tmp/s --token-file $tmp/t --inputs $tmp --outputs $tmp"; do
    code=0
    # shellcheck disable=SC2086 # the arguments are split on purpose
    out=$(sudo -n "$bin" $args 2>&1) || code=$?
    [ "$code" = 2 ] || fail "kbf-guest $args as root exited $code, not 2: $out"
    grep -q "refuses to run as root" <<<"$out" || fail "refused, but not as root: $out"
    echo "refused as expected: $out"
  done
  [ ! -e "$tmp/s" ] || fail "the root run created its socket"
else
  echo "::warning::no passwordless sudo on this runner: the root refusal was not run"
fi

if [ "$(uname -s)" != Darwin ]; then
  echo; echo "kbf-guest root checks passed"
  exit 0
fi

uid=$(id -u)
user=$(id -un)
# plist <label> <stdout file> [UserName]: a job that runs `kbf-guest session` once.
plist() {
  cat <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$1</string>
  <key>ProgramArguments</key><array><string>$bin</string><string>session</string></array>
  <key>StandardOutPath</key><string>$2</string>
  <key>StandardErrorPath</key><string>$2.err</string>
  <key>RunAtLoad</key><true/>
${3:+  <key>UserName</key><string>$3</string>}
</dict>
</plist>
PLIST
}
# wait_for <file>: the job's output, once it has written a line.
wait_for() {
  for _ in $(seq 150); do
    if [ -s "$1" ]; then cat "$1"; return 0; fi
    sleep 0.2
  done
  fail "$1 was never written (stderr: $(cat "$1.err" 2>/dev/null))"
}

step "a LaunchAgent in the runner's GUI session reports Aqua"
launchctl print "gui/$uid" >/dev/null 2>&1 || fail "no gui/$uid domain: this runner has no GUI session"
agent=kbf.ci.guest-session.agent
plist "$agent" "$tmp/agent.out" > "$tmp/agent.plist"
launchctl bootstrap "gui/$uid" "$tmp/agent.plist"
got=$(wait_for "$tmp/agent.out")
launchctl bootout "gui/$uid/$agent" 2>/dev/null || true
echo "LaunchAgent: $got"
[ "$got" = Aqua ] || fail "the LaunchAgent reported '$got', not Aqua"

step "the control: a LaunchDaemon as $user reports System"
if ! $sudo_ok; then
  echo "::warning::no passwordless sudo on this runner: the LaunchDaemon control was not run"
  exit 0
fi
daemon=kbf.ci.guest-session.daemon
plist "$daemon" "$tmp/daemon.out" "$user" > "$tmp/daemon.plist.src"
# launchd loads a system job only from a file root owns and no one else may write.
sudo -n install -o root -g wheel -m 0644 "$tmp/daemon.plist.src" "$tmp/daemon.plist"
sudo -n launchctl bootstrap system "$tmp/daemon.plist"
got=$(wait_for "$tmp/daemon.out")
sudo -n launchctl bootout "system/$daemon" 2>/dev/null || true
echo "LaunchDaemon: $got"
[ "$got" = System ] || fail "the LaunchDaemon reported '$got', not System"

echo; echo "kbf-guest launchd and root checks passed"
