# Spike: macOS P0 mechanics on GitHub-hosted runners

A UI test on a farm Mac rests on a few macOS mechanics that the design marks
**[A]** (assumed) until a probe settles them: `automationmodetool` run as root
without a prompt and kept across users, simulators driven by `simctl` from the
session an action runs in, and the TCC (privacy) state that decides whether a test
may capture the screen ([fleet-updates.md](../design/fleet-updates.md), L1 notes and
the P0 row; [macos-vms.md](../design/macos-vms.md) §11). This spike runs the part of
those probes that a GitHub-hosted macOS runner can run. It is a measurement, not a
decision.

A hosted runner is a virtual machine with passwordless `sudo` and a logged-in runner
user, built from an image GitHub configures. So a hosted result proves the
**mechanics** (the commands, their exact output, that each control arm goes red); it
does not prove what a farm Mac Studio does on its own macOS build, with FileVault on,
with nobody logged in, or with a fresh standard user. The last section lists what
still needs a Studio.

## How to run it

The probes live in `tools/spike/macos/`. The workflow is
`.github/workflows/spike-macos.yml`; it runs by hand (`workflow_dispatch`) and on pull
requests that change the probes. One job per runner label (`macos-26`, `macos-15`)
runs every probe, each in its own step. A Linux job runs the probes' selftest first.

```sh
gh workflow run spike-macos.yml --ref main
gh run view <run id> --log | grep ' SPIKE '
gh run download <run id>      # spike.txt and the simulator screenshot, per job
```

Each probe prints `SPIKE macos-<arch>.<probe>.<arm>=<value>` lines (the `macos-`
prefix keeps them apart from the Linux spike's `<arch>.<key>` lines), one `context`
line (macOS and Xcode builds, user kind, launchd domain, console user) and one
`verdict`:

- **PASS**: the mechanics worked and the control arm went red;
- **FAIL**: the mechanics did not work; the reason follows;
- **BROKEN**: the control arm did not go red, so the probe proved nothing.

Each arm runs in a subshell, so one failed arm is recorded as
`<arm>.status=ERROR(<rc>)` and does not hide the others. The job's last step
(`verdicts.sh`) fails the job on a BROKEN verdict or a missing one, never on a FAIL,
which is a result.

## The probes

| Probe | Arms | Control arm |
|---|---|---|
| `automation_mode.sh` | status as the runner user; `enable-automationmode-without-authentication` under a 60 s watchdog, only while the status says authentication is required; a new standard user added, the status read as that user, the user deleted, the status read again | `disable-automationmode-without-authentication` must bring "requires authentication" back. It runs first when the image starts out not requiring authentication, last otherwise, so both transitions are seen |
| `simctl.sh` (the runner's own session) | newest available iOS runtime and an iPhone device type; `simctl --set <dir>` create, boot, `bootstatus -b` (10 min watchdog), `io screenshot` (must be a PNG), shutdown, delete; then leftover `launchd_sim` processes and device-set entries | a boot under a sandbox profile that denies the lookup of `com.apple.CoreSimulator.CoreSimulatorService` must fail, and the same boot under `(allow default)` alone must succeed |
| `tcc.sh` (instrument for the Screen Recording and Accessibility questions) | SIP status; the ScreenCapture, Accessibility, ListenEvent and PostEvent rows of the system and the user TCC database, each read as the runner user and as root | the same reader over a planted database must return exactly its one known row |

The status text of `automationmodetool` is parsed, case-insensitively, as two fields:
`enabled` (on, off) and `auth` (required, not required), matching the wording the
hosted runners printed (Results). Every reading also records the raw text, and text
the parser does not recognise reads as `unknown`, which never triggers enable and
gives a FAIL. Every `automationmodetool` and `sysadminctl` call runs under the
watchdog with stdin closed; the quick local reads (`id`, the context line) do not.

The selftest (`tools/spike/macos/test/run.sh`) drives each probe against fakes of the
macOS programs. `test/mutants.sh` plants defects that each must turn a test red,
among them: an arm run outside its subshell; a watchdog that leaves the command on
the caller's pipe; `sysadminctl` run without the watchdog; the key without the `macos-` prefix; a
watchdog that reports a hang as success; a status parser that accepts any "require"
as not required; enable run whatever the last status said; a verdict that ignores
the control arm (each probe); a watchdog that leaves stdin open; a simctl verdict
that ignores the `(allow default)` boot, `bootstatus` or a failed delete; error text
from create taken as a UDID; a screenshot not checked to be a PNG; a simulator
booted in the default device set.

## Results

All numbers come from run `37987149882` (commit `95505ed`). Every probe's verdict was
PASS on both labels. Run `37988450842` (commit `51edbb0`, which changed only this
document) gave the same verdicts and the same values.

| | `macos-26` job | `macos-15` job |
|---|---|---|
| macOS | 26.6.2 (25G83), arm64 | 15.7.9 (24G830), arm64 |
| Default Xcode | 26.6 (17F113) | 16.4 (16F6) |
| Context | admin user, launchd domain `Aqua`, console user = the runner user | same |
| SIP | disabled | disabled |

**automationmodetool.** The status prints two lines. The first is `Automation Mode is
disabled.` on every reading. Inferred, not measured: it reports whether a test session
holds Automation Mode now, not the device setting (no reading here ran while a test
session held it). The second is the setting: `This device DOES NOT REQUIRE
user authentication to enable Automation Mode.` or `This device requires user
authentication to enable Automation Mode.` (exact case as printed). Both images start
out not requiring authentication, so the image already ran enable. The control
(`disable-automationmode-without-authentication` via `sudo -n`, stdin closed) brought
`requires` back; `enable-automationmode-without-authentication` the same way took it
to `DOES NOT REQUIRE` again, with exit status 0, no prompt, and well inside the 60 s
watchdog. The first line still said `Automation Mode is disabled.` afterwards: the verb
clears the authentication requirement, it does not turn Automation Mode on. A new standard user, made with `sysadminctl -addUser`, read the same
not-required status, and so did a reading after that user was deleted.
`sysadminctl` warned that the new user cannot use FileVault (no secure token).

The first run of this probe (run `37984178027`, cancelled) ran `sysadminctl` without
the watchdog and with the step's stdin open; neither job got past adding the user in
15 minutes. Since then every `automationmodetool` and `sysadminctl` call runs under the
watchdog with stdin closed, and
the add took about 4 s. What it waited on in that run was not recorded.

**simctl.** The newest available iOS runtime (iOS 26.5 on `macos-26`, iOS 26.2 on
`macos-15`) and an iPhone 17 Pro device type. In a private `--set` device set, create,
boot, `bootstatus -b`, a PNG screenshot of about 2.9 MB, shutdown and delete all
succeeded in the runner's session; `bootstatus` spent its time waiting on data
migration. The control held: under the CoreSimulatorService deny, `simctl` reported
"CoreSimulatorService connection became invalid" and the boot failed, while the same
boot under `(allow default)` succeeded. After the delete: 0 `launchd_sim` processes, 0
entries left in the device set.

**TCC as seeded.** Both TCC databases were readable as the runner user (SIP is off)
and as root. Both images seed the same grants, all `auth_value` 2 (allowed). System
database: ScreenCapture 3 rows, Accessibility 9, PostEvent 3, ListenEvent 0. User
database: ScreenCapture 5, Accessibility 3, PostEvent 1, ListenEvent 0. The granted
clients include `/bin/bash`, `/usr/bin/osascript`, `com.apple.Terminal`,
`com.apple.dt.Xcode-Helper` and the image's own runner and provisioning agents. So on
a hosted runner a shell-launched UI test inherits Screen Recording and Accessibility
grants that a fresh farm user will not have. A hosted UI-test result says nothing
about prompts.

## Not covered here (planned)

- **`xcodebuild test` of a UI test target.** The repository has no UI-test fixture
  yet. A later change adds a macOS app and an iOS app, each with a UI test that takes
  `XCUIScreen.main.screenshot()` and checks a known pattern, plus the TCC log reader
  (`log show` for `com.apple.TCC`, naming the responsible client).
- **simctl from other contexts.** Under kbf's own sandbox profiles, from a system
  LaunchDaemon with no session, and through `launchctl asuser`.
- **The §3.2 probes and `DEVELOPER_DIR` through the driver's `xcode` property.**

## Hosted-runner facts against what needs a Studio

| Question | What a hosted run can settle | What still needs a Mac Studio |
|---|---|---|
| `automationmodetool` as root without a prompt | Settled for hosted macOS 15 and 26: the verbs, the exact status text, that root via `sudo -n` with stdin closed clears the authentication requirement with no prompt (the status still says disabled), and that the control brings authentication back | The same on the Studio's macOS build, run as root at its console or by the provisioning profile, on a Mac where nothing enabled it before |
| The setting across user deletion | Settled as a status reading: a new standard user, and a reading after that user is deleted, still see not required | That a UI test in a fresh standard user's **console session** starts without an authentication sheet: that needs a person to log the user in (FileVault on rules out auto-login) and watch |
| Simulators outside a logged-in session | Settled for the runner's own Aqua session: `simctl` works with a private device set, cleans up after delete, and a sandbox deny of CoreSimulatorService stops it | Whether it works from a LaunchDaemon with nobody logged in, on the Studio's build, with the runtimes installed there |
| Screen Recording need of XCUITest | Only what the image seeded: SIP is off and ScreenCapture is pre-granted to the shell and the runner's agents, so a hosted run cannot answer it | Whether a UI test needs the grant at all, whether a grant persists across users and runner rebuilds: arms in fresh standard users' sessions, with a person to see and click prompts |
| Accessibility | As above, the seeded rows only | The same Studio arms, and the profile route once a Studio is enrolled in device management |
