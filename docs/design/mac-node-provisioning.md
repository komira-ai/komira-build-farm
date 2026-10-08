# Mac node provisioning

**Status: design, for review.** Nothing in this document is built unless it says
"today". It describes how a Mac becomes a kbf worker node and stays one: the baseline
it starts from, the layer that configures it, the artifact it runs, the settings a
headless rack Mac needs, how macOS and Xcode updates are rolled out, how a Mac moves
from a desk to a rack, how it takes over from another farm's worker, and how it
joins and leaves the farm.

It covers the bare-metal host. Simulator, GUI and UI tests run in macOS VMs on that
host (two guests per Mac), and GPU work runs on bare metal with the whole Mac; both are
designed in the [macOS VMs design](https://github.com/komira-ai/komira-build-farm/pull/85) (`docs/design/macos-vms.md`, open PR). Where the two designs touch, this one defers to it:
Xcodes per host and the `xcode` key, simulator runtimes, VM golden images, and GUI
leases.

**One exception: GPU tests drive a GUI on the host itself.** GPU tests never run in a VM
(macOS VMs design, section 8.1); that includes tests that install the desktop app and
drive it with a local LLM. They take the whole Mac on bare metal and need a GUI session
on the host. Their runtime is planned in the [fleet-updates design](https://github.com/komira-ai/komira-build-farm/pull/87) (`docs/design/fleet-updates.md`, open PR): a throwaway per-lease
user, non-admin unless the lease is privileged (`kbf-mac-admin=true`, which always
ends in an erase). A root helper, `kbf-mac-session`, logs that user in automatically
for that lease only. This design does not cover that runtime.

The rules this design keeps:

- **One profile, applied by a script, checked by the same script.** A node is a
  pinned macOS build, plus a profile applied to a freshly erased Mac, plus the
  `kbf-daemon` artifact the deployment installs. Nothing else is on it.
- **No toolchains on a node unless actions need them.** `kbf-daemon` arrives as a
  built, signed and attested `darwin-arm64` artifact from kbf's CI on `main`. It is never
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
| Baseline | A pinned macOS version and build. A fresh state comes from *Erase All Content and Settings*, or from an Apple Configurator restore when the version must change or the Mac is new. There is no disk image for the host; VM golden images do live on Mac nodes (macOS VMs design). |
| Provisioning layer | A plain, idempotent shell script with `apply` and `check` modes, over a declarative key=value profile, that uses only programs macOS ships. Not nix-darwin, not Ansible ([comparison](#2-the-provisioning-layer)). |
| `kbf-daemon` | Built and attested by CI on every commit to `main`, on a hosted macOS arm64 runner: signed (ad-hoc today; Developer ID if the project gets an Apple Developer account), with a SHA-256, an SBOM and build provenance. The operator's deployment job verifies the attestation and installs that per-commit tarball on each node. A release only tags digests that already ran; it never rebuilds. The profile holds host settings, not the daemon's version. |
| Toolchains | The pinned Xcodes (several per host, chosen per action with `DEVELOPER_DIR`, as the macOS VMs design proposes) are in the worker profile. Clients route on `xcode` and `os_build`. Client-defined probes (for example a host identity) run per Xcode and are reported, never matched; an Xcode whose probe value differs from the expected one leaves the report until it re-qualifies. Simulator runtimes live only in VM images. |
| Power | `sleep 0`, `autorestart 1`. The daemon's fence clock stops during sleep or suspend on any OS; that is a code bug ([#78](https://github.com/komira-ai/komira-build-farm/issues/78)), not something a setting fixes ([section 5.1](#51-sleep)). |
| Updates | Automatic download and install off, security responses included. Updates roll out canary-first as a new profile. MDM is decided: Apple Business Manager plus a self-hosted NanoHUB behind `kbf-mdm-gate` (fleet-updates design, section 7; [section 6](#6-updates-pinned-and-rolled-out)). |
| FileVault | Off on rack nodes, so a Mac boots unattended after a power loss. Host auto-login is off at rest: GUI work runs in VM guests that log themselves in. The exceptions are bare-metal GPU tests: for one whole-machine lease, the root helper `kbf-mac-session` sets auto-login to that lease's throwaway user (non-admin unless the lease is privileged), and clears it afterwards (fleet-updates design, open PR). If a VM cannot be started from a launch daemon (an open probe of the macOS VMs design), the fallback is an open decision ([section 5.4](#54-filevault-and-auto-login)). |
| Admin | SSH only, key only, one admin account. `kbf-daemon` runs as a LaunchDaemon under a hidden role account. |
| Join and leave | The node's certificate names its node id ([#79](https://github.com/komira-ai/komira-build-farm/issues/79)). Drain is a protocol message. Short certificate lifetimes and a deny list close the revocation gap ([section 9](#9-joining-and-leaving-the-farm)). |

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
  the macOS build or Xcode yet. ([capabilities.md](capabilities.md) said macOS
  detection was planned; this change brings it up to date.)
- **Platform routing,** under review at the time of writing, matches an action's
  `OSFamily`/`ISA`/`Arch` and kbf keys against node reports. Until it lands, any
  action can go to any registered worker.
- **CI.** The `native-macos` job builds `kbf-daemon` on a hosted macOS runner and runs
  the native driver's tests there. **No workflow publishes a binary for any platform,
  and nothing signs one.**
- **The worker protocol** has no drain message (listed as planned in
  [worker-protocol.md](worker-protocol.md#planned)). The server registers a node by the
  `node_id` its `Hello` carries and **does not check it against the client
  certificate** (`kbf-server`, `worker.rs`, `session`). Any certificate the cell CA
  signed can claim any node id, and because only the newest stream of a worker counts,
  it can take over that node's session. There is no certificate revocation. Tracked in
  [#79](https://github.com/komira-ai/komira-build-farm/issues/79).
- **Fencing.** The daemon fences on `tokio::time::Instant`. Rust's `Instant` is
  `CLOCK_MONOTONIC` on Linux and `CLOCK_UPTIME_RAW` on macOS
  ([std::time::Instant](https://doc.rust-lang.org/std/time/struct.Instant.html)).
  Neither advances while the machine is suspended, so on any OS a node that sleeps
  past T wakes up with its leases still running. Tracked in
  [#78](https://github.com/komira-ai/komira-build-farm/issues/78); see section 5.1.

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
    at the Mac and, if an Apple Account is signed in, that account's password, because
    erasing turns off Activation Lock
    ([Apple](https://support.apple.com/en-us/102664)). So the operator must know whose
    Apple Account, if any, is on each Mac before it can be erased.
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
3. **The profile applied.** That includes the worker profile (section 3). Then the
   deployment installs the `kbf-daemon` artifact (section 4).

There is no image file to build, store or patch for the host. (VM golden images do
live on Mac nodes; they are the macOS VMs design's, not part of the host baseline.)
Rebuilding a node means erase, then apply. The only manual steps are Setup Assistant, which creates the admin account, and
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
| **Testable in kbf's CI** | needs Nix on the hosted runner | needs Ansible on the runner and Python on the target | runs as is on a hosted macOS runner (a VM with passwordless sudo). Some keys can only be proven on a real Mac ([section 10](#10-how-each-part-is-tested)) |
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

`kbf-mac-provision` is one POSIX `sh` script, shipped in the same tarball as
`kbf-daemon` (section 4):

```text
kbf-mac-provision check   --profile FILE   # read-only; one PASS/FAIL per key; exit 1 on any FAIL
kbf-mac-provision apply   --profile FILE   # make every FAIL a PASS; print each change; idempotent
kbf-mac-provision print   --profile FILE   # the profile as resolved, and its SHA-256
```

- **The profile is data:** `KEY=VALUE` lines, parsed and never sourced. An unknown key is
  refused, so a typo fails instead of doing nothing. A few keys (`xcode`, `probe`,
  `expect_probe`) may repeat, one line per item; any other key may appear once. Every value is absolute (`sleep=0`,
  not "turn sleep off"). Nothing in the profile is secret.
- `apply` runs as root. It is run by the operator's deployment job (a CI runner on the
  node, or a CI job over SSH), never by hand on a node once the node exists.
- `apply` ends by running `check`. A run that changes anything prints what it changed;
  a second run must print nothing.
- The node reports `label.profile=<name>@<first 12 hex digits of the profile's
  SHA-256>` (a `--label`), so the server and the farm's UI see which profile each node
  runs.
- **The profile holds host settings only.** The `kbf-daemon` commit and its SHA-256
  are not in it: they are the deployment job's input for each commit it rolls out
  (section 4.3). A daemon upgrade is then not a profile change, and every green
  commit on `main` can deploy without a reviewed edit to each node's profile.

An example profile, with values that are illustrative only:

```text
profile=mac-arm64-pool
macos_version=27.0.1
macos_build=26A000
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
xcode=17A000:/Applications/Xcode-26.6.app/Contents/Developer:<sha256 of the .xip>
xcode=18A000:/Applications/Xcode-27.0.app/Contents/Developer:<sha256 of the .xip>
default_developer_dir=/Applications/Xcode-27.0.app/Contents/Developer
probe=host_identity:<path of the client's probe script>:<sha256 of the script>
expect_probe=host_identity:17A000=26.6-0123456789abcdef
expect_probe=host_identity:18A000=27.0-fedcba9876543210
kbf_server=https://farm.example.net:8981
kbf_cas=https://farm.example.net:8980
kbf_labels=pool=mac rack=r2
```

## 3. Host profile and worker profile

The profile has two halves with different owners and different rates of change.

**The host profile** holds what every Mac node has, whatever its actions need: power,
updates, admin access, accounts, time, logs, the hostname, the scratch volume and the
launchd job that runs `kbf-daemon` (the binary itself is the deployment's, section 4).
It holds **no toolchain**: no Homebrew, no Rust, no Python, no
package manager. `kbf-daemon` links only system libraries, and the provisioning script
needs only what macOS ships.

**The worker profile** holds what actions run with:

- the pinned Xcodes, each at its own path. Several can sit side by side on one host,
  and an action chooses one with `DEVELOPER_DIR`; the node reports them as a set
  (`xcode`, matched by membership). That model, and why, is the macOS VMs design's
  (section 3.2 there). The default developer directory (`xcode-select -p`) is pinned
  too, for actions that name none;
- no simulator runtimes, ever: simulator, GUI and UI tests run in VMs, and their
  runtimes live in VM images only, never on the host;
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
SDK version and still differ in any of these. A client that compiles on Mac nodes may
therefore want a **host identity**: a value its own script computes from the host. What
goes into it is the client's choice; kbf never embeds a client's field list.

For example, komira's script prints the SDK version and a digest of these fields:

| Field | Read with |
|---|---|
| developer directory | `xcode-select -p` |
| SDK version | `xcrun --show-sdk-version` |
| SDK build | `xcrun --show-sdk-build-version` |
| compiler | `cc --version`, first line |
| linker | `ld -v`, first line |
| OS build | `sw_vers -buildVersion` |

Such a value changes on any macOS update (a security response included), any Xcode
update, and any change of developer directory. That is the point of it: a node whose
compilers changed must not keep serving cache keys computed for the old ones.

**Planned, in kbf: client-defined probes, reported, never matched.**

- **Routing keys.** On macOS the daemon reports `os_build` and `xcode`. `xcode` is a
  set, the build of each pinned Xcode, matched by membership (macOS VMs design).
  Clients route on `xcode`, and on `os_build` if their outputs depend on it. Both are
  detected, never typed in, as [capabilities.md](capabilities.md) requires. A Mac has no
  image, and `os_build` is what `os_image` would carry, so a Mac reports `os_build` and
  not `os_image`.
- **Probes.** The node's provisioned config names client probes: `probe.<k>`, a script
  pinned by SHA-256 (for example `probe.host_identity`, a copy of the client's own
  script). The daemon runs each probe once per pinned Xcode, at start and after any
  change to the profile, exactly as an action would run under that Xcode:
  - with the environment cleared;
  - with `DEVELOPER_DIR` set to the same full `…/Contents/Developer` string the daemon
    gives actions that select that Xcode;
  - with `SDKROOT` unset.

  It reports each result in the node's status (`NodeStatus`, fleet-updates design), as
  `probe.<k>` = `<xcode build>=<value>`. A probe is not a node report entry.
  **Assumed, to check on a Mac:** `xcode-select -p` honours `DEVELOPER_DIR`, so the
  developer-directory field differs per Xcode.
- **Status-only.** A `probe.<k>` value is never a request key. The front does not accept
  it as a platform property. A client that wants an identity checks it in its own
  action, as komira's does today, and routes on `xcode`.
- **Expected values.** The provisioned config also names the expected value of each
  probe per Xcode (`--expect-probe host_identity:<xcode build>=<value>`, repeated). The
  daemon compares at start, after each probe run, and before each lease. On a mismatch
  it **stops reporting that Xcode**: its `xcode` value leaves the report and its probe
  values leave the status, and the reason is logged. The node's other Xcodes keep serving. An Xcode that
  changed behind the operator's back drops out instead of serving the wrong compiler.
- **What a client's list holds, and what it costs.** Every platform property is part of
  the action digest:
  - A client matching only `xcode` keeps its Mac cache across macOS patches and loses
    it once per Xcode upgrade.
  - A client matching `os_build` gets a cold Mac cache on **every macOS patch, security
    responses included**. That is correct only when outputs depend on the OS build. A
    test that loads the OS's libraries does; most compiles do not.
  - A client that checks identities in its own actions lists only the identities of the
    Xcode its actions select (for a client that selects none, the default developer
    directory's), never a node's whole set. If the list is part of the actions' inputs
    (komira writes the whole sorted list into every darwin compile), adding or
    removing any listed identity cold-misses that client's darwin cache. Such a client
    batches identity changes into one change per rollout (section 8).

**Re-qualifying an Xcode on a node** whose probe value is about to change:

1. Drain the node (section 9).
2. Apply the new profile. For a first-of-its-kind change, that Xcode's `expect_probe`
   is left empty, so the Xcode is not reported.
3. Run the qualification set on the node, for that Xcode, through a label only the node
   carries:
   - compile, link and run a small C program;
   - the client's canary targets;
   - the native driver's freshness test (a marker left by one lease is never seen by
     the next).
4. Record the value the probe now prints, in the profile's `expect_probe` and in each
   client's list of allowed identities, in the same reviewed change. An identity is
   listed before its host serves, and removed only after the host has stopped serving.
5. Put the node back in its pool label.

The second and later nodes with the same new value skip step 4: their value is already
listed, and step 3 checks that they print it.

## 4. Shipping `kbf-daemon`

### 4.1 Build and attest on `main` (planned)

> **Built since this section was written:** the build and attestation workflow,
> `.github/workflows/artifacts.yml`, described in [docs/artifacts.md](../artifacts.md),
> which is the authority for what it does. Where it differs from this section and 4.2:
>
> - **A separate `attest` job** holds `id-token: write` and `attestations: write`,
>   runs only on a push to `main`, and signs the files the build jobs made. The build
>   jobs hold `contents: read` only, so code a pull request changes never runs next to
>   a token that can sign.
> - **The Linux binaries** (`kbf-daemon` and `kbf-server`, x86_64 and arm64) are built
>   by their own jobs, not by the macOS job.
> - **The darwin tarball holds `kbf-daemon` only:** `kbf-mac-provision` does not exist
>   yet. Only `kbf-daemon` is signed; the fleet-updates helpers do not exist yet.
> - **The oldest macOS supported is 14.0** (`MACOSX_DEPLOYMENT_TARGET`), a choice of
>   that workflow; this design names none.
> - **Signing is ad hoc with the hardened runtime** (`--options runtime`), checked by
>   flag, by running the signed binary, and in behaviour only on a runner with SIP
>   enabled.
> - **Still planned:** the deployment job that verifies an asset before it reaches a
>   node, the node's SHA-256 check in `apply`, and the release workflow.

The model:

- **Every green commit on `main` is built and attested** by CI, and is a candidate for
  deployment. The operator's farm runs these per-commit artifacts.
- **A release only tags.** After a commit's artifacts have run in a production
  deployment for the project's soak period, the release workflow tags them. It checks
  their digests against the attested `main` build and never rebuilds: a rebuild would
  ship bytes that never ran anywhere.

The `main` build job:

- **Runs on a hosted macOS arm64 runner** named by a literal label (the workflow lint
  requires one). It gets `id-token: write` and `attestations: write` for the provenance
  and `contents: read`, and nothing else.
- **Builds** `cargo build --locked --release -p kbf-node --target aarch64-apple-darwin`
  with `MACOSX_DEPLOYMENT_TARGET` set to the oldest macOS kbf supports. That is a
  property of the build, not of the runner's OS.
- **Signs** (section 4.2).
- **Packages** one asset for nodes: `kbf-daemon-<commit>-darwin-arm64.tar.gz`, with the
  binary and `kbf-mac-provision` and no configuration.
  - A `.pkg` is built only if the project takes Developer ID: a signed installer
    package is what an MDM delivers.
  - It is not the node install path, because a pkg receipt keeps naming the installed
    version after a symlink rollback (section 4.3).
- **Sums:** the asset's SHA-256, and one `SHA256SUMS` per commit over every platform's
  asset.
- **SBOM:** a CycloneDX document from `Cargo.lock`, made by a tool pinned by version
  and SHA-256 (as CI pins `cargo-deny`).
- **Provenance:** `actions/attest-build-provenance` for each asset and
  `actions/attest-sbom` for the SBOM. Both are in the allowed `actions/*` set.
- **Keeps** the assets as workflow artifacts. GitHub keeps those for at most 90 days,
  so the release workflow does not rely on them: when it tags a soaked commit, it
  attaches the exact bytes the farm ran (fetched by digest from the deployment's own
  copy, and checked against the attestation) to the release. Nothing is rebuilt.
- **Checks itself:** a last step downloads its own assets and verifies the sums and the
  attestations. A wrong sum fails the job.

The same job builds the Linux binaries. Only binaries this job built are deployed or
released. A build on a developer's machine never ships.

### 4.2 Signing and verification

> **Built since this section was written:** ad-hoc signing with the hardened runtime
> and the attestation checks, in `.github/workflows/artifacts.yml`; see
> [docs/artifacts.md](../artifacts.md) and the deviations listed in section 4.1. The
> verification steps 1 and 2 below are still planned; the `attest` job runs step 1's
> command on its own output.

On Apple silicon every executable must carry a code signature. The linker adds an
*ad-hoc* signature to anything it links for arm64, which is why a `cargo build` on a Mac
runs at all (Apple:
[Big Sur universal apps release notes](https://developer.apple.com/documentation/macos-release-notes/macos-big-sur-11_0_1-universal-apps-release-notes),
[TN2206](https://developer.apple.com/library/archive/technotes/tn2206/_index.html)).

| | Ad-hoc (`codesign -s -`) | Developer ID, hardened runtime, notarized |
|---|---|---|
| Needs | nothing | membership in the Apple Developer Program for an organization, a Developer ID Application certificate (Developer ID Installer for a `.pkg`), and a notary credential, all as secrets of a protected CI environment |
| The kernel checks page hashes against the signature | yes | yes |
| Says *who* built it | no: anyone can ad-hoc sign anything | yes: a Team ID a node can require with `codesign --verify -R '<requirement>'` |
| Gatekeeper, for a file a browser downloaded (quarantined) | refused | allowed |
| Apple's malware scan | no | yes ([notarization](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)) |
| Ticket stapled to the asset | n/a | a `.pkg` only; a bare Mach-O cannot carry a stapled ticket |

A node never gets a quarantined file. The deployment job fetches the asset with a
command-line tool, and **assumed, to verify on the pinned macOS:** command-line
downloads are not quarantined, so Gatekeeper is not consulted. For kbf's own nodes,
Developer ID therefore adds an identity check and a scan. Authenticity comes from
elsewhere in either case:

1. **The deployment job verifies the attestation** before it touches any node. It does
   this in CI, never on the node, which has no `gh`. It pins the signer, the branch and
   the runner type, not just the repository:

   ```text
   gh attestation verify kbf-daemon-<commit>-darwin-arm64.tar.gz \
     --repo <owner>/<repo> \
     --signer-workflow <owner>/<repo>/.github/workflows/<build workflow file> \
     --source-ref refs/heads/main \
     --deny-self-hosted-runners
   ```

   ([gh attestation verify](https://cli.github.com/manual/gh_attestation_verify)).
   Without `--signer-workflow` and `--source-ref`, any workflow of the repository,
   including one on a pull request branch, could produce an asset that verifies.
2. **On the node,** the job hands `apply` the asset and its SHA-256. `apply` compares
   them and refuses on a mismatch before anything is installed. With Developer ID it
   also runs `codesign --verify --strict` against the Team ID requirement.

**Recommendation:** ship ad-hoc signed assets with attestations now. Developer ID is a
decision for the project, since it is a paid account and a signing key to guard. It
mainly helps people who download kbf by hand; kbf's own nodes do not need it. The build
job is written so that adding it is one step, which runs when the signing secret exists.

### 4.3 Install, upgrade and rollback on the node

- The deployment job's input names a commit. The job verifies that commit's asset
  (4.2), then runs on each node, one at a time.
- The binary is installed at `/usr/local/kbf/<commit>/kbf-daemon`.
  `/usr/local/kbf/current` is a symlink to the one in use, and the launchd job runs
  `current/kbf-daemon`. A small file, `/usr/local/kbf/deployed`, records the commit and
  SHA-256 the job installed.
- An upgrade:
  1. stages the new binary beside the old;
  2. drains the node;
  3. switches the symlink;
  4. restarts the job (`launchctl kickstart -k system/<label>`);
  5. waits for the daemon's `welcomed` log line, and switches back if it never comes.

  The previous binary stays on disk until the next upgrade.
- Rollback is the same job with an older commit as its input.
- This is the path until `kbf-updater` ships on Macs (fleet-updates design, phase P1,
  once actions run as lease users). From then on, `kbf-updater` runs `apply` and
  installs `kbf-daemon` from a signed set.
- `check` compares the `current` target's SHA-256 with `/usr/local/kbf/deployed`, so a
  binary changed by hand shows as drift.

## 5. Headless and rack settings

Every row is a profile key. `apply` sets it, and `check` reads it back.

| Setting | Value | Applied with | Why |
|---|---|---|---|
| System sleep | never | `pmset -a sleep 0 standby 0 powernap 0` | capacity: a node that sleeps serves nothing (5.1) |
| Restart after power loss | on | `pmset -a autorestart 1` | a rack power cut must not need a person at each Mac |
| Wake for network access | on | `pmset -a womp 1` | wakes a node that slept anyway. A magic packet reaches only the same L2 segment, not across an overlay network or a router |
| Automatic update download and install | off | `defaults write /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall` = 0 | section 6. **Verify on the pinned version:** the SoftwareUpdate payload is deprecated in macOS 26 and removed in 27 (fleet-updates design, section 7.1) (5.2) |
| Security data files (XProtect and similar) | on | `ConfigDataInstall` = 1 | malware definitions. **Assumed:** they do not change the OS build, and this key still applies separately from `CriticalUpdateInstall` (5.2) |
| App Store auto-update | off | `AutomaticallyInstallAppUpdates` = 0 (the older `com.apple.commerce AutoUpdate` is legacy) | Xcode never comes from the App Store |
| Remote Login | on, key only | `systemsetup -setremotelogin on`, and `/etc/ssh/sshd_config.d/` with `PasswordAuthentication no`, `KbdInteractiveAuthentication no`, `PermitRootLogin no` | the only admin path |
| Screen Sharing, other remote-desktop servers | off, not installed | | SSH is the only way in |
| Firewall | on | `socketfilterfw --setglobalstate on` | `kbf-daemon` never listens. The farm network is the real boundary |
| Network time | on, the farm's time server | `systemsetup -setusingnetworktime on`, `-setnetworktimeserver` | 5.5 |
| Names | one scheme | `scutil --set` `HostName`, `LocalHostName`, `ComputerName` | 5.6 |
| Scratch volume | its own APFS volume, with a quota and indexing off | `diskutil apfs addVolume … -quota`, `mdutil -i off` | a lease cannot fill the system's data volume, and Spotlight does not index lease directories |
| Time Machine, Spotlight on scratch, login items, third-party updaters | off, none | | nothing runs that the profile does not name |

`pmset` keys are as documented in `man pmset` on the pinned macOS; the node's own man
page is the authority for its version. Disk sleep is not set: a Mac's internal SSD has
nothing to spin up.

### 5.1 Sleep

A sleeping node serves nothing, and it drifts from what the scheduler believes about
it. So `sleep 0` is in the profile as a capacity setting.

It is not what keeps fencing correct. A daemon that loses contact with the server
kills its leases once T = 40 s have passed, and the scheduler gives those leases to
another worker after G = 60 s ([worker-protocol.md](worker-protocol.md)). That
argument needs the daemon's clock to count all elapsed time. Rust's `Instant` reads
`CLOCK_MONOTONIC` on Linux and `CLOCK_UPTIME_RAW` on macOS, and neither advances while
the machine is suspended. A node of either OS that sleeps or suspends past T wakes up
believing almost no time has passed, with its leases still running, while the
scheduler may already have placed them elsewhere.

What bounds the harm today:

- The server accepts a result only from the holder of the operation's current lease
  (`kbf-server`, `farm.rs`, `report`), so a stale lease's result is refused and never
  reaches the action cache.
- A heartbeat that lists a lease the scheduler no longer holds gets a `Cancel`.

The harm is duplicate execution. It lasts until the woken node's next heartbeat is
answered or, if the server is unreachable, until its own fence fires T = 40 s later on
a clock that is running again.

**The fix is in code, on every OS**
([#78](https://github.com/komira-ai/komira-build-farm/issues/78)):

- fence against a clock that counts suspend (`CLOCK_BOOTTIME` on Linux;
  `CLOCK_MONOTONIC_RAW` or `mach_continuous_time` on macOS);
- re-check the fence on a short tick;
- check it before sending any `Result`.

A daemon-side "refuse to start unless sleep is off" check would not fix it, and is not
proposed.

### 5.2 Updates are off: what the settings do and do not do

- **They are preferences.** Without MDM, an administrator can change them, and macOS
  can still show upgrade notices. They stop the automatic download and install, and
  `check` reports any change to them as drift. With MDM they can be enforced
  (section 6.3).
- **Verify the keys on the pinned version (27.x).** The `com.apple.SoftwareUpdate`
  payload is deprecated in macOS 26 and removed in 27
  ([SoftwareUpdate](https://developer.apple.com/documentation/devicemanagement/softwareupdate);
  fleet-updates design, section 7.1), in favour of declarative device management.
  Whether the matching local preferences are still honoured on 27 is to verify on a
  node of the pinned version, not assumed.
- **The security split may not hold.** Rapid Security Responses became *Background
  Security Improvements* in macOS 26. **Assumed, to verify:** `CriticalUpdateInstall` =
  0 still stops them, while `ConfigDataInstall` = 1 still allows malware definitions.
  The two may now move together.
- **A security response changes the build.** A Rapid Security Response changed
  `sw_vers -buildVersion` (it appended a letter), and with it any host identity
  (section 3.1). **Assumed, to verify:** a Background Security Improvement does the
  same. If it does, each one is a re-qualification and, for a client that matches
  `os_build`, a cold Mac cache.

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
<key>ProcessType</key>       <string>Interactive</string>
<key>StandardErrorPath</key> <string>/Library/Logs/kbf/kbf-daemon.err.log</string>
<key>SoftResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
<key>HardResourceLimits</key><dict><key>NumberOfFiles</key><integer>65536</integer></dict>
```

- `UserName` is a hidden role account (uid below 500, no login shell, no password),
  never a person's account. Today that account also runs the actions.
- **Per-lease users change this.** Creating and removing a user per lease
  (`sysadminctl`, `dscl`), and starting its processes, needs root, which the role
  account does not have. When per-lease users land, there are two shapes:
  - the daemon runs as root;
  - the daemon keeps the role account and talks to a small root helper, its own
    LaunchDaemon, that does only user creation, removal and spawn.

  The lean is the helper, because it keeps root's surface small.
- **`ProcessType` is `Interactive`.** `launchd.plist(5)` says "Standard jobs are
  equivalent to no ProcessType being set". A job without one gets "light resource
  limitations ... throttling its CPU usage and I/O bandwidth". `Interactive` jobs "run
  with the same resource limitations as apps, that is to say, none". A build node's
  daemon and the actions it starts must not be throttled.
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

**Auto-login is off at rest.** `kbf-daemon` is a LaunchDaemon, and most GUI work runs
in VM leases (`kbf-lease=vm`) whose guests log themselves in (macOS VMs design). The
host needs auto-login in two cases, and in neither for the admin account:

- **Bare-metal GPU tests.** GPU work never runs in a VM (macOS VMs design, section 8.1),
  so a GPU test that drives the desktop app needs a GUI session on the host. The
  planned bare-metal whole-machine runtime makes a throwaway user per lease. That user
  is non-admin, except for a privileged lease (`kbf-mac-admin=true`), whose user may be
  admin and which always ends in an erase.
  - **Who sets auto-login.** A root helper, `kbf-mac-session`, sets auto-login to that
    user for the one lease and clears it afterwards. The unprivileged runtime never
    does.
  - **The switch is not instant.** Each switch costs a userspace restart or a reboot,
    about 1 to 4 minutes.
  - **After a power loss mid-lease,** `kbf-mac-session` resets auto-login at boot,
    before the daemon sends `Hello`.
  - **The same runtime covers** MDM privacy profiles and a leak scan that erases the
    node on a leak.

  All of this is designed in the [fleet-updates design](https://github.com/komira-ai/komira-build-farm/pull/87) (`docs/design/fleet-updates.md`, open PR), not here. FileVault off (above) is
  what makes that auto-login possible. The profile's `autologin` key accepts it:
  `check` passes when auto-login is off, or set to a user in the lease uid range while
  a `whole_machine` lease holds the node, and `apply` never clears it mid-lease.
- **A VM cannot be started from a launch daemon.** This is an open probe of the macOS
  VMs design. If the probe fails, the fallback is an **open decision**: a session of a
  dedicated non-admin VM user, held while the node serves VM leases. That is an
  exception to "auto-login off at rest" and needs a ruling. A per-lease switch is
  impossible, because it restarts userspace under running leases. The `autologin`
  check above, and `kbf-mac-session`'s lease-uid-range rule (fleet-updates design),
  would then also have to accept that user.

One more consequence of running with nobody logged in: **anything the node needs at
boot must be a system daemon.** In particular, an overlay-network client whose app
variant runs only after a user logs in leaves a rebooted node unreachable until someone
logs in. Some vendors ship both a GUI app and a command-line system daemon; a node uses
the daemon.

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
  daemon then owns its own rotation.
- launchd's `StandardErrorPath` file then only catches what is printed before logging
  starts (bad flags, a panic), and stays small. **It is not rotated.** launchd opens it
  once and keeps the descriptor, so a renamed file keeps receiving writes, and
  `newsyslog` has no copy-and-truncate mode. Instead, the deployment job truncates it
  while the job is stopped, at each restart and upgrade.
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

- One macOS version and build per pool, and its pinned Xcodes, named in the profile.
- How the server rolls host updates out (macOS and Xcode, MDM, the update UI) is
  designed in the [fleet-updates design](https://github.com/komira-ai/komira-build-farm/pull/87)
  (`docs/design/fleet-updates.md`, open PR).
- Nothing installs by itself (section 5.2).
- "Update available" is computed by the server (fleet-updates design, section 3.2). It
  reports Xcode and macOS together, because each Xcode sets a minimum macOS: "Xcode X is
  out; it needs macOS Y; the pool runs Z."
- A person decides. The rollout is a new profile version:
  1. One canary node: drain, update macOS first if needed, then Xcode, re-apply,
     re-qualify (section 3.1), back in the pool.
  2. The rest one at a time, each drained, so the pool loses one node at a time.
  3. An old Xcode is removed when no action asks for it any more.
- A security fix takes the same path, sooner.

### 6.2 Without MDM (a fallback until MDM is live)

This path is only a fallback, until the MDM of section 6.3 is live on the nodes.

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
  `SystemPeriodInDays`); and enforcement of a
  specific version at a set time
  ([settings](https://support.apple.com/guide/deployment/software-update-settings-declarative-dep0578d8b8a/web),
  [enforcement](https://support.apple.com/guide/deployment/install-and-enforce-software-updates-depd30715cbb/web)).
  These need Device Enrollment or Automated Device Enrollment, and supervision for
  most keys. Background Security Improvements are not covered by these keys: what MDM
  can and cannot do about them is in the fleet-updates design, section 7.2.
- **The bootstrap token,** which on Apple silicon authorises update installs and
  erasing without a person
  ([Apple](https://support.apple.com/guide/deployment/use-secure-and-bootstrap-tokens-dep24dbdcf9e/web)).

What it costs: an Apple Business Manager organization (it needs a legal entity's
D-U-N-S number); Macs bought through Apple or an authorized reseller, or added by hand
with Apple Configurator; an MDM service to buy or run, open-source options included; and
a push certificate to renew every year.

**Decided: MDM.** Apple Business Manager plus a self-hosted NanoHUB behind
`kbf-mdm-gate`, as the fleet-updates design (section 7) specifies. The profile does not
change: an MDM delivers the same script as a package. Section 6.2 is the fallback until
it is live.

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

The binding and the deny list below are tracked in
[#79](https://github.com/komira-ai/komira-build-farm/issues/79).

1. `check` passes on the node: the profile is applied, and the identity is the
   expected one.
2. The node makes its key pair itself; the private key never leaves the node. Its CSR
   names the node id. The cell CA signs a certificate for that node id with a short
   lifetime (30 days is a starting point), as an operator step for now, and later
   through an enrollment endpoint.
3. The operator adds the node to the cell's configuration: node id, labels, profile
   name, expected probe values per Xcode.
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
  never runs on two nodes at once. A node that suspended is the exception until
  [#78](https://github.com/komira-ai/komira-build-farm/issues/78) is fixed (section
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
| `kbf-mac-provision` | on a hosted macOS runner: `apply` with a test profile, `apply` again prints no change, `check` passes. Then change one setting by hand (`AutomaticDownload` = 1, a name) and `check` must fail naming it | a `check` that skips a key; an `apply` that is not idempotent (writes on every run) |
| Profile parsing | unknown key, duplicate key, a value with `$(...)` | a profile that is sourced; an unknown key ignored |
| Build job | its last step re-downloads its own assets and verifies sums and attestations with the flags of 4.2 | one flipped byte in the asset before the check; an attestation from another workflow file accepted |
| Node install | `apply` with a wrong SHA-256 refuses before touching `/usr/local/kbf` | the sum compared after the switch, or not at all |
| `--expect-probe` | two Xcodes with fixture probe outputs; one probe's value changes. That Xcode's `xcode` value and probe entry leave the report, and the other Xcode's stay | the whole node dropped on one mismatch; the mismatched Xcode still reported; the probe run with the daemon's environment or `SDKROOT` set instead of an action's |
| Fence clock ([#78](https://github.com/komira-ai/komira-build-farm/issues/78)) | an injected clock that jumps forward, as a resume does: the lease is killed and no `Result` is sent | the fence on a clock that does not count suspend |
| Node-id binding ([#79](https://github.com/komira-ai/komira-build-farm/issues/79)) | server tests: a certificate for node A sending a first `Hello` as node B is refused; on an established stream, a resent `Hello` whose `node_id` differs from the stream's worker is refused and ends the stream (today a resent `Hello` only updates capacity and its `node_id` is never read) | the first-`Hello` check removed; a resent `node_id` ignored rather than refused |
| Drain | scheduler simulation: a drained node gets no new lease; running leases finish or are re-placed after the deadline | placement that ignores the drained flag |
| Deny list | a denied serial's `Hello` is refused, also on reconnect | the list read only at server start |

**What a hosted runner can and cannot prove.** Hosted macOS runners are virtual
machines.

- **Exercised there:**
  - profile parsing;
  - the software-update and App Store preferences;
  - `sshd_config.d`;
  - the three names;
  - the firewall;
  - the role account and the launchd job;
  - the scratch volume and its quota;
  - the tarball install, with its SHA-256 refusal and the `current` switch.
- **Only on a real node of the pinned version.** The operator runs these as a canary
  checklist on each new profile version:
  - `pmset` sleep, `autorestart` and `womp`, which a VM may accept and ignore;
  - FileVault state;
  - Remote Login (`systemsetup` needs Full Disk Access, and the runner's own access
    depends on it);
  - the time server;
  - whether the update preferences are honoured on that version (5.2);
  - the Xcode install from a `.xip`.

## 11. Verified and assumed

| Claim | Status |
|---|---|
| The daemon's flags, LaunchDaemon fit, SIGTERM behaviour, macOS report entries, native driver gaps | read in the code on `main` |
| No workflow publishes or signs a binary | read in `.github/workflows` |
| The server does not tie node id to certificate; no revocation | read in `kbf-server` and `worker.proto` |
| A result is accepted only from the current lease holder | read in `kbf-server` (`farm.rs`, `report`) |
| `Instant` is `CLOCK_MONOTONIC` on Linux and `CLOCK_UPTIME_RAW` on macOS | Rust documentation |
| Neither clock advances during suspend | Linux `clock_gettime(2)`, macOS `clock_gettime(3)`. Not tested on a node |
| Erase All Content and Settings keeps the macOS version and needs the signed-in Apple Account's password; a Configurator restore installs from an IPSW | Apple documentation |
| An old macOS version that Apple no longer signs cannot be restored | **assumed** |
| FileVault: SSH unlock on macOS 26 or later; the bootstrap token does not unlock at boot; no auto-login with FileVault | Apple documentation |
| Storage is encrypted without FileVault, keyed to the Secure Enclave | Apple documentation |
| `ProcessType`: unset or `Standard` is throttled; `Interactive` is not | `launchd.plist(5)` |
| The SoftwareUpdate payload is deprecated in macOS 26 and removed in 27 | Apple's device-management schema (fleet-updates design, section 7.1); whether the local preferences are still honoured on 27 is **to verify** |
| `CriticalUpdateInstall` = 0 stops Background Security Improvements while `ConfigDataInstall` = 1 keeps definitions | **assumed**; verify on the pinned version |
| A Background Security Improvement changes `sw_vers -buildVersion` | **assumed** (a Rapid Security Response did); verify |
| Command-line downloads are not quarantined, so Gatekeeper is not consulted | **assumed**; verify on the pinned version |
| launchd keeps the `StandardErrorPath` descriptor; `newsyslog` cannot copy and truncate | `launchd.plist(5)`, `newsyslog.conf(5)` |
| A macOS update on Apple silicon needs a volume owner's authorisation without MDM | **assumed**; verify |
| Unattended Xcode downloads with an Apple Account are not reliable | **assumed** |
| `xcode-select -p` honours `DEVELOPER_DIR`, so a probe run per Xcode sees that Xcode's directory | **assumed**; check on a Mac |
| DDM software-update keys and their enrollment requirements | Apple documentation |
| Some overlay-network clients' app variants run only after login | vendor documentation |

## 12. Decisions for the project

1. **Developer ID signing and notarization:** take a paid Apple Developer account and
   guard a signing key, or ship ad-hoc signed assets with attestations (the lean).
2. **MDM: decided.** Apple Business Manager plus a self-hosted NanoHUB behind
   `kbf-mdm-gate` (fleet-updates design, section 7).
3. **FileVault on rack nodes:** off (the lean), or on, with a person to unlock every
   node after every reboot. On Apple silicon, turning it off only re-wraps the volume
   key (the data stays hardware-encrypted); it is not a long decryption.
4. **The macOS pin and how often it moves:** which version and build each pool starts
   on, and whether security updates go to the canary within a set number of days.
5. **Where `kbf-mac-provision` lives:** in this repository, in each per-commit tarball
   and tested on hosted runners (the lean, so anyone running kbf on Macs gets it), or in
   each operator's own deployment.
