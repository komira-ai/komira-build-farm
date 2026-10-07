# Mac node provisioning

**Status: design, for review.** Nothing in this document is built unless it says
"today". It describes how a Mac becomes a kbf worker node and stays one: the baseline
it starts from, the layer that configures it, the artifact it runs, the settings a
headless rack Mac needs, how macOS and Xcode updates are rolled out, how a Mac moves
from a desk to a rack, how it takes over from another farm's worker, and how it
joins and leaves the farm.

The rules this design keeps:

- **One profile, applied by a script, checked by the same script.** A node is a
  pinned macOS build, plus a profile applied to a freshly erased Mac, plus the
  `kbf-daemon` artifact the profile names. Nothing else is on it.
- **No toolchains on a node unless actions need them.** `kbf-daemon` arrives as a
  built, signed and attested `darwin-arm64` artifact from kbf's CI. It is never
  compiled on the node. The toolchains actions use (Xcode or the Command Line Tools,
  at pinned versions) belong to the node's *worker profile*, and nothing on the host
  side may depend on them.
- **Nothing updates by itself.** macOS and Xcode change only when the operator rolls
  out a new profile, one canary node first. A node whose toolchain identity changed
  must re-qualify before it serves again.
- **A desk Mac and a rack Mac run the same profile.** Moving one re-applies it, with
  other labels.

## Summary of the recommendations

| Topic | Recommendation |
|---|---|
| Baseline | A pinned macOS version and build. A fresh state comes from *Erase All Content and Settings*, or from an Apple Configurator restore when the version must change or the Mac is new. There is no disk image. |
| Provisioning layer | A plain, idempotent shell script with `apply` and `check` modes, over a declarative key=value profile, that uses only programs macOS ships. Not nix-darwin, not Ansible ([comparison](#2-the-provisioning-layer)). |
| `kbf-daemon` | Built by a release job on a hosted macOS arm64 runner, signed (ad-hoc today; Developer ID if the project gets an Apple Developer account), with SHA-256 sums, an SBOM and build provenance. A node pins the version and SHA-256 in its profile. |
| Toolchains | One pinned Xcode (or one CLT version) per pool, in the worker profile. The node reports its host identity, and a changed identity takes the node out until it re-qualifies. |
| Power | `sleep 0` (also needed for correctness: the daemon's fence clock stops while a Mac sleeps), `autorestart 1`, `womp 1`. |
| Updates | Automatic download and install off, security responses included. Updates roll out canary-first as a new profile. MDM is optional ([section 6](#6-updates-pinned-and-rolled-out)). |
| FileVault | Off on rack nodes, so a Mac boots unattended after a power loss. Auto-login stays off until GUI leases need it ([section 5.4](#54-filevault-and-auto-login)). |
| Admin | SSH only, key only, one admin account. `kbf-daemon` runs as a LaunchDaemon under a hidden role account. |
| Join and leave | The node's certificate names its node id. Drain is a protocol message. Short certificate lifetimes and a deny list close the revocation gap ([section 9](#9-joining-and-leaving-the-farm)). |

## What kbf has today

This design builds on the code on `main`, read at the time of writing.

- **The binary.** `kbf-daemon` lives in crate `kbf-node` and takes
  `--driver fake|container|native`. On a Mac it runs `--driver native` with `--server`,
  `--cas`, `--ca-cert`, `--cert`, `--key`, `--node-id`, `--scratch` and repeatable
  `--label key=value` (reported as `label.<key>`). It is configured by flags only. On
  SIGTERM it abandons running leases (their processes are killed and their directories
  removed) and exits zero.
- **The native driver** (`kbf-driver-native`) runs each action as a process tree in a
  fresh lease directory. It keeps the network off with `sandbox-exec` unless the action
  asks for it, and denies job submission to launchd and Launch Services opens. It
  watches memory over the whole tree, clears the `uchg`/`uappnd` flags an action set
  before removing its directory, and sweeps directories a crashed daemon left. **Actions
  run as the daemon's user.** The crate documentation lists what that leaves open: an
  action can read the daemon's files, its TLS private key included, and can schedule
  work outside its tree (a `LaunchAgents` plist, `at`, `cron`). The fix it names is a
  per-lease user, which is **planned**.
- **Detection.** On macOS the daemon reads `sysctl` and reports `os=macos`, `arch=arm64`,
  `cpus`, `mem_gib`, `page_size`, `gpu=0`, `cpu.model`, every `cpu.features` flag and
  `isa_level`, `drivers`, and the driver's `network_isolation`. It reports nothing about
  the macOS build or Xcode yet. ([capabilities.md](capabilities.md) still says a
  non-Linux daemon refuses to start and that macOS detection is planned; the code on
  `main` detects macOS. That document should be brought up to date.)
- **Platform routing,** under review at the time of writing, matches an action's
  `OSFamily`/`ISA`/`Arch` and kbf keys against node reports. Until it lands, any
  action can go to any registered worker.
- **CI.** The `native-macos` job builds `kbf-daemon` on a hosted macOS runner and runs
  the native driver's tests there. **No workflow publishes a binary for any platform,
  and nothing signs one.**
- **The worker protocol** has no drain message (listed as planned in
  [worker-protocol.md](worker-protocol.md#planned)). The server registers a node by the
  `node_id` its `Hello` carries and **does not check it against the client
  certificate**. Any certificate the cell CA signed can claim any node id, and because
  only the newest stream of a worker counts, it can take over that node's session.
  There is no certificate revocation.
- **Fencing.** The daemon fences on `tokio::time::Instant`. On macOS, Rust's `Instant` is
  `clock_gettime(CLOCK_UPTIME_RAW)`
  ([std::time::Instant](https://doc.rust-lang.org/std/time/struct.Instant.html)), and
  per macOS's `clock_gettime(3)` that clock does not advance while the system sleeps.

## 1. The baseline

### 1.1 Why a Mac is not imaged

A Linux node can be installed from a disk image. An Apple silicon Mac cannot, in any
supported way:

- It boots only an Apple-signed macOS. The system volume is a sealed, signed snapshot,
  and the boot loader verifies the seal before it starts the kernel
  ([Signed system volume security](https://support.apple.com/guide/security/signed-system-volume-security-secd698747c9/web)).
  A customised system volume cannot be written to many Macs.
- There is no network boot. The supported ways to put a Mac in a known state are:
  - **Erase All Content and Settings** erases data, settings and apps and keeps the
    installed macOS version. It takes minutes. It needs an administrator's credentials
    at the Mac
    ([Apple](https://support.apple.com/en-us/102664)).
  - **An Apple Configurator restore** over a USB-C cable in DFU mode. It replaces the
    firmware and recoveryOS, erases the internal storage, and installs macOS from an
    IPSW file. It needs a second Mac running Apple Configurator
    ([Apple](https://support.apple.com/en-us/108900)).
  - **`startosinstall --eraseinstall`** from a full installer. It needs a volume owner's
    credentials on Apple silicon.
- A restore is personalised by Apple's signing server for each Mac. **Assumed, not
  verified:** a macOS version Apple no longer signs cannot be restored, so a pin can
  only name a version that Apple still signs. A pinned version stays installed once
  in place; the limit applies to bringing a new or repaired Mac to an old pin.

### 1.2 What the baseline is

A node's baseline is three things, all of them named in the profile:

1. **A pinned macOS:** product version *and* build (`sw_vers -productVersion` and
   `-buildVersion`). The build is the stronger pin: a security response changes the
   build without changing the version a person reads.
2. **A fresh state.** Erase All Content and Settings on a Mac already at the pinned
   version. Use an Apple Configurator restore for a new Mac, a Mac at another version,
   or a Mac that will not boot. A Mac that was a workstation is always erased: a node
   is clean only if nothing was ever installed on it that the profile does not name.
3. **The profile applied.** That includes the worker profile (section 3) and the
   `kbf-daemon` artifact (section 4).

There is no image file to build, store or patch. Rebuilding a node means erase, then
apply. The only manual steps are Setup Assistant, which creates the admin account, and
turning on Remote Login. Without MDM both need a person at the Mac once; with automated
device enrollment, neither does (section 6.3).

## 2. The provisioning layer

The profile is applied by something. The three candidates:

| | nix-darwin | Ansible | Plain idempotent script |
|---|---|---|---|
| **What runs on the node** | the Nix daemon, the Nix store under `/nix` (its own APFS volume), the nix-darwin activation | Python on the node for every module except `raw` ([Ansible](https://docs.ansible.com/ansible/latest/installation_guide/intro_installation.html#managed-node-requirements)). macOS's `/usr/bin/python3` comes from the Command Line Tools or Xcode | `/bin/sh` and programs macOS ships (`pmset`, `defaults`, `launchctl`, `dscl`, `scutil`, `systemsetup`, `softwareupdate`, `installer`, `pkgutil`, `shasum`, `codesign`) |
| **Reproducibility** | best for what Nix manages (store paths by hash). macOS settings are still `defaults` writes in activation scripts | as good as the playbook's modules. Idempotence per module | as good as our predicates. We write the idempotence |
| **Drift detection** | rebuilds and compares the system closure. Settings outside Nix (pmset, softwareupdate) are not watched | `--check --diff`, where modules support check mode | `check` mode: the same predicates `apply` uses, read-only, one PASS/FAIL line per setting |
| **Bootstrap on a fresh Mac** | install Nix (a volume, a daemon, a build-users group), then nix-darwin. Survives a macOS update only if the `/nix` volume and `synthetic.conf` entry survive (a known source of trouble; assumed, not tested) | the CLT install (for Python) before anything else, or `raw` steps to get there | copy one script and one profile file, run `apply` |
| **Rollback** | generations: switch to the previous one | re-run with the previous playbook. No built-in undo | re-run `apply` with the previous profile version. Every setting is absolute, never a delta |
| **Fit with "no toolchains on nodes"** | poor: a package manager and a store on every node, and actions can find `/nix/store` | poor: the provisioner's runtime is the action toolchain's Python, so updating Xcode changes the provisioner, and provisioning cannot run before Xcode exists | good: the provisioner needs nothing the worker profile installs, and is the same before and after an Xcode change |
| **Fit with "CI deploys; no new deployment tool"** | a new tool | a new tool | a script a CI job runs, like the rest of a deployment |
| **Testable in kbf's CI** | needs Nix on the hosted runner | needs Ansible on the runner and Python on the target | runs as is on a hosted macOS runner, which has passwordless sudo |
| **Familiarity** | Nix is a language of its own | widely known | shell, like kbf's existing CI scripts |

**Recommendation: the plain idempotent script.** nix-darwin is the most reproducible
for packages, but a node has almost no packages. What it has are macOS settings, a
launchd job, an account, a signed binary and Xcode, and nix-darwin manages those with
the same `defaults` and `launchctl` calls the script would make, after putting a
package manager on every node. Ansible's model fits well, but on a Mac its runtime is
the action toolchain's Python. The script costs us its idempotence, which we must write
and test ourselves. The check mode and the tests in [section 10](#10-how-each-part-is-tested)
are how that cost is paid.

### 2.1 Shape of the script (planned)

`kbf-mac-provision` is one POSIX `sh` script, shipped in the same release as
`kbf-daemon` (section 4):

```text
kbf-mac-provision check   --profile FILE   # read-only; one PASS/FAIL per key; exit 1 on any FAIL
kbf-mac-provision apply   --profile FILE   # make every FAIL a PASS; print each change; idempotent
kbf-mac-provision print   --profile FILE   # the profile as resolved, and its SHA-256
```

- **The profile is data:** `KEY=VALUE` lines, parsed and never sourced. An unknown key is
  refused, so a typo fails instead of doing nothing. Every value is absolute (`sleep=0`,
  not "turn sleep off"). Nothing in the profile is secret.
- `apply` runs as root. It is run by the operator's deployment job (a CI runner on the
  node, or a CI job over SSH), never by hand on a node once the node exists.
- `apply` ends by running `check`. A run that changes anything prints what it changed;
  a second run must print nothing.
- The node reports `label.profile=<name>@<first 12 hex digits of the profile's
  SHA-256>` (a `--label`), so the server and the farm's UI see which profile each node
  runs.

An example profile, with values that are illustrative only:

```text
profile=mac-arm64-pool
macos_version=26.5
macos_build=25A000
hostname=kbf-mac-07
admin_user=farmadmin
role_user=_kbf
power_sleep=0
power_autorestart=1
power_womp=1
updates_auto_download=0
updates_auto_install_macos=0
updates_auto_install_security=0
updates_config_data=1
appstore_auto_update=0
remote_login=1
ssh_password_auth=0
filevault=off
autologin=off
firewall=on
time_server=time.example.net
scratch_volume=kbf
scratch_quota_gib=2048
xcode_version=26.5
xcode_build=17A000
xcode_xip_sha256=<sha256 of the .xip>
developer_dir=/Applications/Xcode-26.5.app/Contents/Developer
host_identity=26.5-0123456789abcdef
kbf_daemon_version=0.4.0
kbf_daemon_sha256=<sha256 of the release asset>
kbf_server=https://farm.example.net:8981
kbf_cas=https://farm.example.net:8980
kbf_labels=pool=mac rack=r2
```

## 3. Host profile and worker profile

The profile has two halves with different owners and different rates of change.

**The host profile** holds what every Mac node has, whatever its actions need: power,
updates, admin access, accounts, time, logs, the hostname, the scratch volume and the
`kbf-daemon` artifact. It holds **no toolchain**: no Homebrew, no Rust, no Python, no
package manager. `kbf-daemon` links only system libraries, and the provisioning script
needs only what macOS ships.

**The worker profile** holds what actions run with:

- exactly one Xcode, or exactly one Command Line Tools version, per pool. A pool
  needs Xcode.app if any of its actions use `xcodebuild` or simulators. The developer
  directory `xcode-select -p` prints is pinned too, because it decides which `cc` and
  SDK an action gets;
- the simulator runtimes, if the pool runs simulator tests;
- nothing else. Tools that come bundled with Xcode or the CLT (`git`, `python3`,
  `make`) are part of the worker profile because that is where they come from. The
  host side must not use them.

Xcode is installed from the `.xip` whose SHA-256 the profile names, and never from the
App Store. Getting the `.xip` onto a node needs an Apple Account sign-in, which
renewing needs a person for. **Assumed:** unattended developer downloads are not
reliable. So the download is an operator step once per Xcode version; installing and
verifying it is the script's.

### 3.1 Host identity

A compile on macOS uses more of the host than its inputs name: the compiler driver, the
linker, the SDK, and the OS whose libraries a test loads. Two Macs can print the same
SDK version and still differ in any of these. A client that compiles on Mac nodes can
therefore pin a **host identity**: a digest over the fields below. komira's build does
this, listing allowed identities and refusing to compile anywhere else.

| Field | Read with |
|---|---|
| developer directory | `xcode-select -p` |
| SDK version | `xcrun --show-sdk-version` |
| SDK build | `xcrun --show-sdk-build-version` |
| compiler | `cc --version`, first line |
| linker | `ld -v`, first line |
| OS build | `sw_vers -buildVersion` |

The identity changes on any macOS update (a security response included), any Xcode or
CLT update, and any `xcode-select` switch. That is the point of it: a node whose
compilers changed must not keep serving cache keys computed for the old ones.

**Planned, in kbf:**

- The daemon reports `os_build`, `xcode` (the Xcode build, or the CLT version) and
  `host_identity` (the digest above) in its node report. They are detected, never
  typed in, as [capabilities.md](capabilities.md) requires. A client that wants an
  exact match sends `host_identity` as a platform property. Then the identity is part
  of the action digest, which is correct: the output depends on it.
- `kbf-daemon --expect-host-identity VALUE` (the profile's `host_identity`). At start,
  and before each lease, the daemon compares the detected identity with the expected
  one. On a mismatch it refuses new leases and reports why. A Mac that changed behind
  the operator's back drops out of the pool instead of serving the wrong compiler.

**Re-qualifying a node** whose identity is about to change:

1. Drain the node (section 9).
2. Apply the new profile. Its `host_identity` is left empty for a first-of-its-kind
   change.
3. Run the qualification set on the node through a label only it carries: compile,
   link and run a small C program; the client's canary targets; the native driver's
   freshness test (a marker left by one lease is never seen by the next).
4. Record the identity the node now prints in the profile, and in each client's list
   of allowed identities, in the same reviewed change.
5. Put the node back in its pool label.

The second and later nodes with the same new identity skip step 4: their identity is
already listed, and step 3 checks that they print it.

## 4. Shipping `kbf-daemon`

### 4.1 The release job (planned)

No workflow publishes a binary today. The release job:

- **Runs on a hosted macOS arm64 runner** named by a literal label (the workflow lint
  requires one). It runs with `contents: write` for the release, and `id-token: write`
  and `attestations: write` for the provenance, and nothing else.
- **Builds** `cargo build --locked --release -p kbf-node --target aarch64-apple-darwin`
  with `MACOSX_DEPLOYMENT_TARGET` set to the oldest macOS kbf supports. That is a
  property of the release, not of the runner's OS.
- **Signs** (section 4.2).
- **Packages** two assets: `kbf-daemon-<version>-darwin-arm64.tar.gz` (the binary and
  `kbf-mac-provision`), and `kbf-daemon-<version>-darwin-arm64.pkg`, built with
  `pkgbuild` with a fixed identifier. The `.pkg` installs to a versioned path, and its
  receipt (`pkgutil --pkg-info`) gives `check` the installed version. Neither asset
  holds configuration.
- **Sums:** one `SHA256SUMS` over every asset of the release, every platform.
- **SBOM:** a CycloneDX document from `Cargo.lock`, made by a tool pinned by version
  and SHA-256 (as CI pins `cargo-deny`).
- **Provenance:** `actions/attest-build-provenance` for each asset and
  `actions/attest-sbom` for the SBOM. Both are in the allowed `actions/*` set, and
  `gh attestation verify` checks the result
  ([gh attestation verify](https://cli.github.com/manual/gh_attestation_verify)).
- **Checks itself:** a last step downloads the release's own assets and checks the
  sums and the attestations. A wrong sum fails the release.

The same job builds the Linux binaries. Only binaries built by this job are released.
A build on a developer's machine is never shipped.

### 4.2 Signing

On Apple silicon every executable must carry a code signature. The linker adds an
*ad-hoc* signature to anything it links for arm64, which is why a `cargo build` on a Mac
runs at all (Apple:
[Big Sur universal apps release notes](https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes),
[TN2206](https://developer.apple.com/library/archive/technotes/tn2206/_index.html)).

| | Ad-hoc (`codesign -s -`) | Developer ID, hardened runtime, notarized |
|---|---|---|
| Needs | nothing | membership in the Apple Developer Program for an organization, a Developer ID Application certificate (Developer ID Installer for the `.pkg`), and a notary credential, all as secrets of a protected CI environment |
| The kernel checks page hashes against the signature | yes | yes |
| Says *who* built it | no: anyone can ad-hoc sign anything | yes: a Team ID a node can require with `codesign --verify -R '<requirement>'` |
| Gatekeeper, for a file a browser downloaded (quarantined) | refused | allowed |
| Apple's malware scan | no | yes ([notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)) |
| Ticket stapled to the asset | n/a | the `.pkg` only; a bare Mach-O cannot carry a stapled ticket |

A node never gets a quarantined file. The deployment job fetches the asset with a
command-line tool, and **assumed, to verify on the pinned macOS:** command-line
downloads are not quarantined, so Gatekeeper is not consulted. For kbf's own nodes,
Developer ID therefore adds an identity check and a scan, and authenticity comes from
elsewhere in either case:

1. The operator's deployment pins `kbf_daemon_sha256` in the profile, in a reviewed
   change, after `gh attestation verify` passed for that asset. The check runs in CI
   or on an admin machine, never on the node, which has no `gh`.
2. On the node, `apply` downloads the asset, compares its SHA-256 with the pin, and
   refuses on a mismatch before anything is installed. With Developer ID, it also runs
   `codesign --verify --strict` against the Team ID requirement.

**Recommendation:** release ad-hoc signed assets with attestations now. Developer ID is
a decision for the project, since it is a paid account and a signing key to guard. It
mainly helps people who download kbf by hand, and kbf's own nodes do not need it. The
release job is written so that adding it is a step behind a secret's presence.

### 4.3 Install, upgrade and rollback on the node

- Installed at `/usr/local/kbf/<version>/kbf-daemon`. `/usr/local/kbf/current` is a
  symlink to the version in use, and the launchd job runs `current/kbf-daemon`.
- An upgrade stages the new version beside the old, drains the node, switches the
  symlink, restarts the job (`launchctl kickstart -k system/<label>`), and waits for
  the daemon's `welcomed` log line. If it never comes, it switches back. The previous
  version stays on disk until the next upgrade.
- `check` compares the `current` target with `kbf_daemon_version` and the file's
  SHA-256 with `kbf_daemon_sha256`.

## 5. Headless and rack settings

Every row is a profile key. `apply` sets it, and `check` reads it back.

| Setting | Value | Applied with | Why |
|---|---|---|---|
| System sleep | never | `pmset -a sleep 0 standby 0 powernap 0` | capacity, and **correctness** (5.1) |
| Disk sleep | never | `pmset -a disksleep 0` | lease I/O never waits for a spin-up |
| Restart after power loss | on | `pmset -a autorestart 1` | a rack power cut must not need a person at each Mac |
| Wake for network access | on | `pmset -a womp 1` | lets an operator wake a node that slept anyway |
| Automatic update download and install | off | `defaults write /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall` = 0 | section 6 |
| Security data files (XProtect and similar) | on | `ConfigDataInstall` = 1 | malware definitions. **Assumed:** they do not change the OS build |
| App Store auto-update | off | `com.apple.commerce AutoUpdate` = 0 | Xcode never comes from the App Store |
| Remote Login | on, key only | `systemsetup -setremotelogin on`, and `/etc/ssh/sshd_config.d/` with `PasswordAuthentication no`, `KbdInteractiveAuthentication no`, `PermitRootLogin no` | the only admin path |
| Screen Sharing, other remote-desktop servers | off, not installed | | SSH is the only way in |
| Firewall | on | `socketfilterfw --setglobalstate on` | `kbf-daemon` never listens. The farm network is the real boundary |
| Network time | on, the farm's time server | `systemsetup -setusingnetworktime on`, `-setnetworktimeserver` | 5.5 |
| Names | one scheme | `scutil --set` `HostName`, `LocalHostName`, `ComputerName` | 5.6 |
| Scratch volume | its own APFS volume, with a quota and indexing off | `diskutil apfs addVolume … -quota`, `mdutil -i off` | a lease cannot fill the system's data volume, and Spotlight does not index lease directories |
| Time Machine, Spotlight on scratch, login items, third-party updaters | off, none | | nothing runs that the profile does not name |

`pmset` keys are as documented in `man pmset` on the pinned macOS
([archived man page](https://developer.apple.com/library/archive/documentation/Darwin/Reference/ManPages/man1/pmset.1.html)).
The node's own man page is the authority for its version.

### 5.1 Why sleep must be off

A daemon that loses contact with the server kills its leases once 40 s (T) have
passed, and the scheduler gives those leases to another worker after 60 s (G)
([worker-protocol.md](worker-protocol.md)). Both sides measure elapsed time, and the
protocol holds because the daemon's T expires before the server's G. On macOS, the
daemon's clock is `CLOCK_UPTIME_RAW`, which stops during sleep. A Mac that sleeps for
ten minutes wakes up believing almost no time has passed. Meanwhile the server has
already re-run its leases elsewhere. So `sleep 0` is a precondition of the fencing
argument on macOS, not just a capacity setting. **Planned:** the daemon reads the
`pmset` sleep setting at start and refuses `--driver native` on a Mac whose system sleep
is not 0. Separately, the daemon's fence could move to a clock that counts sleep
(`CLOCK_MONOTONIC`, which macOS documents as counting it). That change is worth making
whatever the setting; it is a code change tracked apart from this design.

### 5.2 Updates are off: what the settings do and do not do

Without MDM these are preferences. An administrator can change them, and macOS can
still show upgrade notices. They stop the automatic download and install, and `check`
reports any change to them as drift. With MDM they can be enforced (section 6.3).

### 5.3 The launchd job

`kbf-daemon` runs as a **LaunchDaemon** (`/Library/LaunchDaemons`), not a
LaunchAgent. An agent runs only inside a user's login session and stops at logout. A
daemon starts at boot with nobody logged in
([Creating launch daemons and agents](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)).

```xml
<key>Label</key>             <string>org.example.kbf-daemon</string>
<key>ProgramArguments</key>  <array><string>/usr/local/kbf/current/kbf-daemon</string> <!-- flags from the profile --></array>
<key>UserName</key>          <string>_kbf</string>
<key>GroupName</key>         <string>_kbf</string>
<key>RunAtLoad</key>         <true/>
<key>KeepAlive</key>         <true/>
<key>ThrottleInterval</key>  <integer>10</integer>
<key>ExitTimeOut</key>       <integer>30</integer>
<key>ProcessType</key>       <string>Standard</string>
<key>StandardErrorPath</key> <string>/Library/Logs/kbf/kbf-daemon.err.log</string>
<key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
<key>HardResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
```

- `UserName` is a hidden role account (uid below 500, no login shell, no password),
  never a person's account. Today that account also runs the actions. A per-lease user
  will change that (**planned**, see "What kbf has today").
- `ProcessType` must not be `Background`. **Assumed:** a background job's processes,
  and the actions they start, get a lower scheduling class and are kept on the
  efficiency cores.
- `ExitTimeOut` gives the daemon time to kill its leases on SIGTERM before launchd
  sends SIGKILL.
- The server's address, the node id, the labels and the certificate paths are flags in
  `ProgramArguments`, written from the profile. No environment variables.

### 5.4 FileVault and auto-login

On an Apple silicon Mac the internal storage is always encrypted. With FileVault off,
the volume key is protected only by the Secure Enclave's hardware key. With FileVault
on, it is also bound to a user's password
([Volume encryption with FileVault](https://support.apple.com/guide/security/volume-encryption-with-filevault-sec4c6dc1b6e/web)).
FileVault therefore protects data on a Mac that was taken whole, and nothing else.

| | FileVault on | FileVault off |
|---|---|---|
| After a power loss | the Mac waits at the unlock screen. No launchd daemon runs until someone unlocks it. On macOS 26 or later it can be unlocked over SSH, which still needs a person with a password ([Intro to FileVault](https://support.apple.com/guide/deployment/intro-to-filevault-dep82064ec40/web)) | boots and rejoins by itself |
| Planned reboot (an update) | `fdesetup authrestart` with an admin's password or the recovery key at that moment | a plain restart |
| MDM bootstrap token | authorises update installs and erasing; does **not** unlock at boot ([Apple](https://support.apple.com/guide/deployment/use-secure-and-bootstrap-tokens-dep24dbdcf9e/web)) | n/a |
| Auto-login | not available ([Apple](https://support.apple.com/en-us/102316)) | available |
| A stolen Mac | data protected by a password | data readable by whoever boots it |

**Recommendation: FileVault off on rack nodes.** A rack node must come back after a
power cut without a person. What a node holds is transient lease data and one client
key, which is revocable and short-lived (section 9.4). The compensating controls are
the rack's physical security and short certificate lifetimes. A Mac on a desk, outside
a locked room, may keep FileVault on if the operator accepts that it waits for a person
after every reboot.

**Auto-login** is not needed: `kbf-daemon` is a LaunchDaemon. It becomes necessary only
for leases that drive a GUI session, which belong to the whole-machine lease design. If
that design needs it, auto-login is for a non-admin lease account, never the admin
account.

One more consequence of running with nobody logged in: **anything the node needs at
boot must be a system daemon.** In particular, an overlay-network client whose app
variant runs only after a user logs in (for example, the app variants of Tailscale; the
command-line `tailscaled` runs before login,
[Tailscale](https://tailscale.com/docs/concepts/macos-variants)) leaves a rebooted node
unreachable until someone logs in.

### 5.5 Time

macOS keeps time with `timed`, against `time.apple.com` unless told otherwise. A node
uses the same time source as the farm's servers. Time matters for certificate validity
(a node whose clock is far off fails the TLS handshake), for reading logs from several
machines side by side, and for the timestamps in `ExecutedActionMetadata`. It does not
matter for fencing, which uses each side's own monotonic clock. `check` queries the
server with `sntp` (read-only) and fails above an offset the profile names.

### 5.6 Names

- One name per node, used everywhere: `HostName`, `LocalHostName` and `ComputerName`
  are set to the same value. That value is the daemon's `--node-id` and the name in
  its client certificate.
- Lower-case ASCII letters, digits and `-`: `<farm>-mac-<nn>` (`kbf-mac-07`). Never a
  person's name, and nothing with an apostrophe or a space.
- **No location in the name.** Where a node is (desk, rack, slot) is a label
  (`--label rack=r2`), which changes when the Mac moves. The name does not, so a move
  does not change the node's identity, its certificate or its history.

### 5.7 Logs (partly planned)

Today `kbf-daemon` writes human-readable `tracing` lines to stderr, and launchd writes
stderr to the file `StandardErrorPath` names.

- **Planned:** `--log-format json` and `--log-dir DIR` (daily files, a kept count). The
  daemon then owns its own rotation. **Assumed:** launchd opens `StandardErrorPath`
  once, so a file rotated underneath it keeps receiving writes under its old name.
  launchd's file then only catches what is printed before logging starts (bad flags, a
  panic) and stays small. `newsyslog` (`/etc/newsyslog.d/kbf.conf`) bounds it.
- **Shipping:** one log shipper, chosen by the operator, tails `--log-dir` and forwards
  to the farm's log store. It is infrastructure, not a toolchain, and arrives the way
  `kbf-daemon` does: a pinned artifact checked by SHA-256, as a LaunchDaemon under its
  own account.
- Not in the daemon's log: actions' stdout and stderr, which go to the CAS with the
  result. Crash reports of the daemon are in `/Library/Logs/DiagnosticReports`. When a
  node is drained after a failure, `log show` for the failure window (memory pressure,
  jetsam, kernel) is collected with it.

## 6. Updates: pinned and rolled out

### 6.1 The policy

- One macOS version and build, and one Xcode, per pool, named in the profile.
- Nothing installs by itself (section 5.2).
- A scheduled check outside the farm notices new Apple releases and opens a review
  item. It reports Xcode and macOS together, because each Xcode sets a minimum macOS: "Xcode X is out; it needs macOS Y; the pool runs Z."
- A person decides. The rollout is a new profile version:
  1. One canary node: drain, update macOS first if needed, then Xcode, re-apply,
     re-qualify (section 3.1), back in the pool.
  2. The rest one at a time, each drained, so the pool loses one node at a time.
  3. The old Xcode is removed when no node uses it.
- A security fix takes the same path, sooner.

### 6.2 Without MDM

- `softwareupdate --install <label> --restart` on a drained node. On Apple silicon,
  installing a macOS update needs a volume owner's authorisation. Without MDM that is
  an admin's password, given at the node or through `--stdinpass` (`man
  softwareupdate`), so the job that runs it holds an admin password as a secret.
  **Assumed:** this applies on the pinned version, and must be verified there.
- Nothing stops an administrator from undoing the settings. `check` notices.

### 6.3 With MDM

Apple Business Manager plus an MDM service adds:

- **Automated device enrollment:** a new Mac skips Setup Assistant and enrolls on its
  first boot, so no step needs a person at the Mac.
- **Software update control through declarative device management:** `AutomaticActions`
  (`Download`, `InstallOSUpdates`, `InstallSecurityUpdates`) can be set `AlwaysOff`;
  deferrals of 1 to 90 days (`MajorPeriodInDays`, `MinorPeriodInDays`,
  `SystemPeriodInDays`); `RapidSecurityResponse` `Enable`; and enforcement of a
  specific version at a set time
  ([settings](https://support.apple.com/guide/deployment/software-update-settings-declarative-dep0578d8b8a/web),
  [enforcement](https://support.apple.com/guide/deployment/install-and-enforce-software-updates-depd30715cbb/web)).
  These need Device Enrollment or Automated Device Enrollment, and supervision for
  most keys.
- **The bootstrap token,** which on Apple silicon authorises update installs and
  erasing without a person
  ([Apple](https://support.apple.com/guide/deployment/use-secure-and-bootstrap-tokens-dep24dbdcf9e/web)).

What it costs: an Apple Business Manager organization (it needs a legal entity's
D-U-N-S number); Macs bought through Apple or an authorized reseller, or added by hand
with Apple Configurator; an MDM service to buy or run, open-source options included; and
a push certificate to renew every year.

**Lean:** none for a handful of Macs. The script does the same work, and a person
enters a password once per update per node. Adopt MDM when Macs arrive in batches or
when updates become frequent enough that the password step costs more than running an
MDM. The profile does not change either way: an MDM delivers the same script as a
package.

## 7. From the desk to the rack

The move is **the same profile, re-applied**. Nothing in the profile knows whether the
Mac is on a desk or in a rack, except its labels.

1. Drain the node (section 9) and stop its daemon.
2. If the Mac was ever used as a workstation, or carries anything the profile does not
   name, erase it (section 1.2). This is the usual case for a desk Mac.
3. Rack it: wired network only (Wi-Fi off), power through the rack's PDU, no display.
   With MDM, it enrolls on first boot. Without MDM, the one-time Setup Assistant and
   Remote Login steps need a display and keyboard once.
4. Apply the profile with the rack's labels (`rack=<id>`). The name, node id and
   certificate do not change (section 5.6).
5. Re-qualify (section 3.1), whether or not the identity changed: a moved Mac proves
   itself before serving.
6. Back in its pool label.

Desk habits that must not reach the profile: Wi-Fi, a logged-in user, a GUI-only
overlay-network client, a person's accounts, and remote-desktop servers.

## 8. Coexisting with another REAPI farm's worker, and the cutover

A Mac may already be a worker of another REAPI farm, for example a worker running as a
per-user LaunchAgent. Running kbf beside it on the same Mac is possible, and unsafe:

- Each farm books the whole machine. kbf's booking does not see the other farm's
  actions, so both fill the same cores and memory. The native driver's memory limit
  then kills leases that were within their booking.
- kbf has no way yet to keep part of a machine for something else ("protected floors"
  in [capabilities.md](capabilities.md#planned) are planned).

The options:

| Option | Cost |
|---|---|
| **One Mac, one farm** (recommended): move Macs one at a time | the pool is split, so both farms run on fewer Macs during the move |
| Time-share: stop one worker while the other runs | manual and error-prone, and the stopped farm's queue waits |
| Both at once, each capped in its own configuration | needs kbf's protected floors (planned) and the other farm's slot counts. Not before floors exist |

The cutover of one Mac, using the first option:

1. **Choose the order.** Move first the Mac whose loss the other farm feels least.
2. **Drain the other farm's worker** in that farm's own way: stop new work and let
   running actions finish, then unload its launchd jobs. Keep its files (binaries,
   configuration, job definitions) for rollback.
3. **Re-provision:** erase (section 1.2) or apply, then join kbf with the pool label
   (section 9) and re-qualify (section 3.1).
4. **Shadow:** send a sample of the client's Mac actions to kbf (a second build
   configuration), and compare results and times with the other farm.
5. **Move the client's Mac lane** to kbf once kbf's Mac capacity carries it. The lane
   is the client's configuration that names the remote execution endpoint for its Mac
   platform.
6. **Move the remaining Macs** one at a time, as in steps 2 and 3.
7. **Rollback** at any step: stop `kbf-daemon` (`launchctl bootout`), reload the other
   farm's jobs from the kept files, and point the client's lane back. Until the Mac is
   erased, nothing is lost. After an erase, rollback means re-installing the other
   worker.

A client that pins host identities (section 3.1) must have each Mac's new identity in
its list before that Mac serves it. Re-provisioning usually changes the identity. Both
farms' lists and the client's are updated in the same reviewed change.

## 9. Joining and leaving the farm

### 9.1 Today

- **Join:** the daemon presents a client certificate the cell CA signed, sends
  `Hello` with its node id and report, and is placed on according to its report.
- **Leave:** SIGTERM. Running leases are killed and their directories removed. The
  scheduler gives them up after G = 60 s and places them again elsewhere.
- **Identity:** not tied to the certificate. **Revocation:** none.

### 9.2 Join (planned)

1. `check` passes on the node: the profile is applied, and the identity is the
   expected one.
2. The node makes its key pair itself; the private key never leaves the node. Its CSR
   names the node id. The cell CA signs a certificate for that node id with a short
   lifetime (30 days is a starting point), as an operator step for now, and later
   through an enrollment endpoint.
3. The operator adds the node to the cell's configuration: node id, labels, profile
   name, expected host identity.
4. The daemon starts. **The server checks that the certificate names the node id the
   `Hello` carries** and refuses the stream otherwise. Then a certificate taken from
   one node can impersonate only that node.
5. The node serves only its qualification label until re-qualification passes, then
   gets its pool label.

### 9.3 Drain and leave (planned)

- **Drain** is a worker-protocol message (listed as planned in
  [worker-protocol.md](worker-protocol.md#planned)). The server stops placing new
  leases on the node. Running leases finish, up to a drain deadline; past it they are
  cancelled and placed again. The node reports when it holds no lease. An
  operator's drain goes through the server, so it works the same for a deployment job
  and for a person.
- Until the message exists, the stand-in is to stop the daemon. Running leases are
  lost and re-run after G.
- **Fencing during a leave:** a node that disappears keeps running its leases until its
  own fence fires at T = 40 s; the scheduler gives them away at G = 60 s; an action
  never runs on two nodes at once. On a Mac this holds only with sleep off (section
  5.1).
- **Deregistration:** remove the node from the cell configuration, delete its key on the
  node, and add its certificate serial to the server's **deny list** (planned; checked at
  `Hello`). A certificate that is no longer used still verifies until it expires, so
  the deny list is what closes the gap, and a short lifetime bounds it if the list is
  missed.

### 9.4 The node's secret

A node holds one secret: its client private key. Object-store credentials stay on the
servers; daemons reach the CAS only through the server.

- **Who can read it today:** the daemon's user, and therefore every action, because
  actions run as that user. The native driver documents this.
- **What a stolen key allows:** connecting as that node, receiving the leases placed on
  it (and with them the source in their inputs), and returning results that the server
  writes to the action cache. So the key is high value.
- **What limits it:** the node-id binding (9.2), short lifetimes, the deny list (9.3),
  and the per-lease user (planned in the native driver), after which actions cannot read
  the daemon's files at all.
- **Later:** a key in the System keychain or the Secure Enclave, which cannot be
  exported, used through a signing callback in the TLS stack. It is not needed for the
  first version.

## 10. How each part is tested

kbf's rule holds here: every test has been seen failing on a planted defect.

| Part | Test | Mutant it must catch |
|---|---|---|
| `kbf-mac-provision` | on a hosted macOS runner (passwordless sudo): `apply` with a test profile, `apply` again prints no change, `check` passes. Then change one setting by hand (`pmset -a sleep 10`, `AutomaticDownload` = 1) and `check` must fail naming it | a `check` that skips a key; an `apply` that is not idempotent (writes on every run) |
| Profile parsing | unknown key, duplicate key, a value with `$(...)` | a profile that is sourced; an unknown key ignored |
| Release job | its last step re-downloads its own assets and verifies sums and attestations | one flipped byte in the asset before the check |
| Node install | `apply` with a wrong `kbf_daemon_sha256` refuses before touching `/usr/local/kbf` | the sum compared after the switch, or not at all |
| `--expect-host-identity` | fixture fields, a changed `os_build` | comparing only the SDK version prefix |
| Sleep refusal | fixture `pmset -g` output with `sleep 10` | the check reading `displaysleep` |
| Node-id binding | a server test: a certificate for node A sending `Hello` as node B is refused | the binding not checked on a resent `Hello` |
| Drain | scheduler simulation: a drained node gets no new lease; running leases finish or are re-placed after the deadline | placement that ignores the drained flag |
| Deny list | a denied serial's `Hello` is refused, also on reconnect | the list read only at server start |

## 11. Verified and assumed

| Claim | Status |
|---|---|
| The daemon's flags, LaunchDaemon fit, SIGTERM behaviour, macOS report entries, native driver gaps | read in the code on `main` |
| No workflow publishes or signs a binary | read in `.github/workflows` |
| The server does not tie node id to certificate; no revocation | read in `kbf-server` and `worker.proto` |
| `Instant` on macOS is `CLOCK_UPTIME_RAW` | Rust documentation |
| `CLOCK_UPTIME_RAW` stops during sleep | macOS `clock_gettime(3)`. Not tested on a node |
| Erase All Content and Settings keeps the macOS version; a Configurator restore installs from an IPSW | Apple documentation |
| An old macOS version that Apple no longer signs cannot be restored | **assumed** |
| FileVault: SSH unlock on macOS 26 or later; the bootstrap token does not unlock at boot; no auto-login with FileVault | Apple documentation |
| Storage is encrypted without FileVault, keyed to the Secure Enclave | Apple documentation |
| `CriticalUpdateInstall` = 0 stops automatic security responses; `ConfigDataInstall` does not change the OS build | **assumed**; verify on the pinned version |
| Command-line downloads are not quarantined, so Gatekeeper is not consulted | **assumed**; verify on the pinned version |
| A `Background` launchd job's actions are kept on efficiency cores | **assumed**; measure |
| launchd keeps `StandardErrorPath` open across rotation | **assumed** |
| A macOS update on Apple silicon needs a volume owner's authorisation without MDM | **assumed**; verify |
| Unattended Xcode downloads with an Apple Account are not reliable | **assumed** |
| DDM software-update keys and their enrollment requirements | Apple documentation |
| The app variants of one common overlay-network client run only after login | that vendor's documentation |

## 12. Decisions for the project

1. **Developer ID signing and notarization:** take a paid Apple Developer account and
   guard a signing key, or ship ad-hoc signed assets with attestations (the lean).
2. **MDM:** none for now (the lean), or Apple Business Manager plus an MDM.
3. **FileVault on rack nodes:** off (the lean), or on, with a person to unlock every
   node after every reboot.
4. **The macOS pin and how often it moves:** which version and build each pool starts
   on, and whether security updates go to the canary within a set number of days.
5. **Where `kbf-mac-provision` lives:** in this repository, shipped with each release
   and tested on hosted runners (the lean, so anyone running kbf on Macs gets it), or in
   each operator's own deployment.
