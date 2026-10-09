# macOS VM guests: setup, capture, network, identity and the host side

This document fills in what [macos-vms.md](macos-vms.md) leaves open about a VM lease:
how a golden image gets past Setup Assistant with a logged-in test user and the
`kbf-guest` agent in it, how screenshots and videos leave a guest when the host has
no window server, what network a guest gets, how an image is named when every Mac
may have to build its own, which uid runs the VM, how the VM helper is signed, and
which probes on a real Mac must answer the open questions before code relies on
them. **Everything in it is planned.** No VM code exists on `main`: there is no
`kbf-driver-vm`, `kbf-vmm` or `kbf-guest` crate.

Claims about Apple's software are marked **[V]** (read in the source linked) or
**[A]** (an assumption, or a fact nobody has checked on our hardware), as in
macos-vms.md. A **lean** is the option this document recommends; it is not decided
until the matching row of [section 10](#10-open-decisions) is.

## 1. Summary

| Question | Lean |
|---|---|
| How the image gets past Setup Assistant | The host writes two things into the freshly installed guest disk: a first-boot launch daemon and Apple's setup-done marker. On the first boot the daemon, as the guest's root, creates the test user and does every root step, then removes itself and shuts the guest down ([section 2](#2-guest-first-boot-setup)). |
| Who runs root steps in a lease's guest | Nobody. Root work happens once, at image build. In a lease the only kbf process is `kbf-guest`, a LaunchAgent of the non-admin test user. |
| Screenshots and video | Captured inside the guest (XCTest attachments in the `.xcresult`, `simctl io`, `screencapture`), written into the action's outputs on the read-write share, collected on the host by `kbf-outputs` ([section 3](#3-display-and-capture)). The host never sees the screen. |
| Network | Off by default: no network device at all, only the virtio socket. `network=on` would mean one NAT device; it is refused for VM leases until a LAN-deny design exists ([section 4](#4-networking)). |
| What `vm.image` names | A **recipe digest**: the SHA-256 of the pinned inputs. Each node also records a **content digest** of the bytes it built, which attests the disk but is not routed on ([section 5](#5-image-identity)). |
| Where images come from | Built on each Mac from the same recipe. Shipping built images between Macs is an open decision for the project's owner, after a legal review ([section 5.4](#54-distribution-and-the-licences)). |
| Which uid runs the VM | A throwaway lease user from `kbf-mac-session user-create`, started with `run`; the driver talks to it over the descriptors `run` already passes ([section 6](#6-the-run-as-uid-and-the-control-channel)). |
| Signing | `kbf-vmm` alone carries `com.apple.security.virtualization`, through a per-binary entitlement allow list in the signing scripts ([section 7](#7-signing-with-the-virtualization-entitlement)). |
| The biggest risk | Whether Virtualization.framework starts a macOS guest from a launchd daemon's child with nobody logged in on the host ([section 8](#8-the-launch-daemon-risk)). |

## 2. Guest first-boot setup

### 2.1 What has to be in the image

After `VZMacOSInstaller` finishes, the guest disk holds a fresh macOS that boots into
Setup Assistant **[A]**. Before any lease can use it, the image needs:

| Item | Why | Needs root in the guest |
|---|---|---|
| A non-admin test user, `kbf-test` | the session XCUITest and the simulator run in | yes (creating a user) |
| Setup Assistant done, system-wide and for that user (`/var/db/.AppleSetupDone` and the per-user first-login panes) | no pane waits for a click | yes for the system marker **[A]** |
| Auto-login of `kbf-test` (`/etc/kcpassword` and `autoLoginUser` in `com.apple.loginwindow`) | the guest reaches an Aqua session with nobody at it. Cirrus's templates do exactly this ([vanilla-tahoe.pkr.hcl](https://github.com/cirruslabs/macos-image-templates/blob/main/templates/vanilla-tahoe.pkr.hcl)) **[V]** | yes |
| Sleep, screen saver and screen lock off; automatic updates off | a lease never finds a locked or sleeping display, and the image does not change itself. The same template turns sleep and the screen saver off with `systemsetup -setsleep Off` and `com.apple.screensaver` defaults **[V]** | yes |
| `kbf-guest` as a LaunchAgent, in `/Library/LaunchAgents`, owned by root, limited to the Aqua session of `kbf-test` | the agent the driver talks to ([macos-vms.md](macos-vms.md#6-the-vm-driver-one-vm-per-lease-never-reused), step 4) | yes (a root-owned plist) |
| `DevToolsSecurity -enable` and `kbf-test` in the `_developer` group | without them, starting and attaching to a test host can raise an administrator prompt nobody can answer **[A]** | yes |
| `automationmodetool enable-automationmode-without-authentication` | UI automation starts without a person; its manual says running it "requires an administrator to authenticate in the shell" ([automationmodetool(1)](https://keith.github.io/xcode-man-pages/automationmodetool.1.html)) **[V]** | yes |
| Xcode: expanded from the pinned `.xip`, `xcode-select -s` to it, `xcodebuild -runFirstLaunch`, the Metal toolchain component, the pinned simulator runtimes | the guest's toolchain ([macos-vms.md](macos-vms.md#7-images) step 3); `xcode-select` points at the full Xcode, as on the hosts | yes **[A]** for `-runFirstLaunch` and system-wide runtime installs |
| `xcodebuild -license accept` | Xcode's tools refuse to run until it is accepted **[A]** | yes. Whether an automated image build may accept it is an open decision ([section 10](#10-open-decisions)) |
| A login keychain for `kbf-test` with a known password | some XCTest flows unlock it, and a prompt would hang the test **[A]** | no |
| Simulator devices, created once per runtime | the guest is fresh for every lease, so devices built into the image are fresh too; no `simctl create` per lease | no |
| Privacy (TCC) grants for capture, only if probe P9 shows they are needed | [section 3](#3-display-and-capture) | see 2.4 |

### 2.2 Options for getting past Setup Assistant

| Option | How | Verdict |
|---|---|---|
| A. Keystrokes over VNC | Cirrus's Packer templates type through Setup Assistant with a timed `boot_command` (`"<wait30s>italiano<esc>english<enter>"` and so on, same template) **[V]**. | Not usable. Virtualization.framework has no public VNC server **[A]**; the host side sees the screen only through `VZVirtualMachineView`, an `NSView` ([VZVirtualMachineView](https://developer.apple.com/documentation/virtualization/vzvirtualmachineview)) **[V]**, which needs a window server the daemon does not have. Keystroke timing is also fragile across macOS releases. |
| B. Enrol the guest in the MDM | The MDM's Setup Assistant payload skips panes for new users ([fleet-updates.md](fleet-updates.md#102-isolation-layers)). | Not usable alone: a VM has no serial number in Apple Business Manager, so automated enrolment does not apply to it **[A]**. |
| C. Write everything offline from the host | Attach the guest's disk image on the host, mount its Data volume, write the user record, the plists and the files, detach. | Possible, but a macOS user record is more than a file (a shadow hash, a generated UID, group membership); writing it by hand is fragile across releases **[A]**, and every file must be root-owned in the guest, which needs root on the host or a mount that honours owners **[A]**. |
| **D. A first-boot launch daemon (lean)** | Write only two things offline: `/var/db/.AppleSetupDone`, and a launch daemon plist plus its script under `/Library`. On the first boot the guest's launchd runs the script as the guest's root. It does every row of 2.1 with Apple's own tools (`sysadminctl`, `dseditgroup`, `defaults`, `systemsetup`, `pmset`, `DevToolsSecurity`, `automationmodetool`, `xcode-select`, `xcodebuild`), writes the kbf marker (2.3), deletes itself and shuts the guest down. | Lean. The offline part is two small files, so the host-side root question shrinks to one: can a host user without root write a root-owned file into the attached guest volume (probe P0)? If not, a narrow root helper verb or the MDM writes those two files on the host; that is then a separate decision ([section 10](#10-open-decisions)). |

The first-boot daemon is the guest's, not the host's. It runs inside the VM being
built, never on a farm Mac, and it no longer exists in any image a lease boots. The
host-side rule of [fleet-updates-security.md](fleet-updates-security.md), that
`automationmodetool` runs once from the host's provisioning profile and not through
the root helper, is about the host and does not change.

### 2.3 The build, step by step, and the setup-done markers

1. **Install.** `kbf-vmm image build <recipe>` runs `VZMacOSInstaller` with the pinned
   restore image.
2. **Offline write.** Attach the disk, write the two files of option D, detach.
3. **First boot.** The first-boot daemon runs the setup. Each step's command and exit
   status go to a log on the read-write share, so a failed build says which step
   failed. Its last step writes `/var/db/kbf/setup-done`: the recipe digest
   ([section 5](#5-image-identity)) and one line per step with its result. Then it
   removes itself and shuts down.
4. **Verification boot.** The guest auto-logs in and `kbf-guest` starts. Through it,
   the builder checks, as `kbf-test`: the session is Aqua (`launchctl managername`
   prints `Aqua`) **[A]**; `xcode-select -p` is the pinned Xcode; `xcrun simctl list
   runtimes` is the recipe's list; `DevToolsSecurity -status` is enabled;
   `automationmodetool` reports automation mode on without authentication; and the
   marker's recipe digest is this recipe's. Any mismatch fails the build.
5. **Stop** and compute the manifest and its content digest ([section 5.2](#52-the-content-digest-attests-the-bytes)).

Two markers, two jobs: Apple's `.AppleSetupDone` keeps Setup Assistant away; kbf's
`setup-done` says which recipe set the image up and that every step passed. At the
start of every lease, `kbf-guest` reports the marker's recipe digest, and the driver
refuses a guest whose digest is not the one the action asked for (an infrastructure
failure with the reason, not the action's).

### 2.4 Privacy grants in the guest

Screen Recording cannot be granted by a configuration profile; at most a standard user
may approve it without an administrator's password, and the click remains
([fleet-updates.md](fleet-updates.md#102-isolation-layers)) **[V]**. In a guest that
click can be made once, at image build, only by writing the guest's TCC database. That
database is protected by System Integrity Protection while the guest runs **[A]**, so it
could only be written offline in step 2, and Apple may treat a hand-written row as
tampering **[A]**. So: probe P9 first finds out whether any capture kbf needs requires
the grant at all. If none does, the image carries no TCC rows. If one does, writing the
row offline is tried in P9 too, and the result decides whether that capture is offered.

## 3. Display and capture

### 3.1 No host window server

`kbf-daemon` runs as a launch daemon, which "is not allowed to connect to the window
server" (macos-vms.md section 2.1) **[V]**. The only public way for the host to show a
guest's screen is `VZVirtualMachineView`, an `NSView` (above) **[V]**. So the host
never looks at a guest's screen, and every capture happens inside the guest.

The guest still gets a graphics device, `VZMacGraphicsDeviceConfiguration` with one
display at a fixed size the recipe names (for example 1920 × 1200), and no view on
the host. Without a display the guest's window server would have nothing to draw on
**[A]**. The display size is part of the recipe, so screenshots from two leases on the
same image are the same size.

### 3.2 What captures what

| Capture | Tool, inside the guest | Needs a Screen Recording grant | Where it lands |
|---|---|---|---|
| XCUITest screenshots and attachments, simulator or macOS app | XCTest, written into the result bundle | **[A]**, P9 | the `.xcresult` bundle |
| A simulator's screen | `xcrun simctl io <device> screenshot <file>` and `recordVideo <file>` **[A]** for the exact verbs on the pinned Xcode | **[A]**, P9 (the same question is open for bare metal in fleet-updates.md) | a file the action names |
| The whole guest screen, for a macOS app | `screencapture` | yes **[A]** | a file the action names |

The action asks for all of these itself: they are commands in its argv, and their files
are its declared outputs. kbf adds no capture of its own.

### 3.3 How outputs leave the guest

`macos-vms.md` section 6 plans two virtiofs shares, one read-only and one read-write.
The lean puts the action's execution root on the **read-write** share:

- The host materializes the execution root (the input tree and the parents of the
  declared outputs) into the lease directory, as the native driver does, and shares it
  read-write. The guest works in it directly, so tools that write next to their inputs
  behave as on bare metal.
- The **read-only** share carries only control files: the per-boot token the guest
  agent checks (section 6.2), the setup-done check's expected digest, and nothing an
  action writes.
- The guest cannot reach anything in the lease directory outside these two shares,
  so stdout and stderr travel over the control channel (section 6.2): `kbf-guest`
  sends them as framed chunks over the virtio socket, and `kbf-vmm` writes them to
  two files in the lease directory, outside the execution root, stopping at the
  stdout and stderr limits below and marking the stream as cut. A third share for
  them is not needed. After the run, the host side collects the declared outputs
  with `kbf-outputs`, which follows no symlinks, exactly as for a bare-metal lease.

A shared directory is read-only to the guest when the host makes it so
(`VZSharedDirectory`'s `readOnly`,
[VZSharedDirectory](https://developer.apple.com/documentation/virtualization/vzshareddirectory))
**[V]**. Whether virtiofs is fast enough for the build steps a UI test runs before it
tests is measured in P4 **[A]**; if it is not, the fallback is a copy of the inputs onto
the guest's own disk, paid once per lease.

Sizes: on `main`, `kbf-outputs` allows an action 16 GiB of output files and 1 GiB each of
stdout and stderr by default (`OutputLimits::DEFAULT`,
`crates/kbf-outputs/src/limits.rs`) **[V]**, which holds a `.xcresult` and a few videos
of hundreds of megabytes. Large outputs go through the cache's normal upload; chunked
upload of large blobs is planned in [storage.md](storage.md) and helps here too.

A live view of a running guest for an operator (Screen Sharing inside the guest,
reached over the virtio socket, off by default) is not designed and is later work.

## 4. Networking

An action asks for the network with the platform property `network` on `main`:
absent, `off` or `none` keeps it off; `on` or `standard` turns it on
(`crates/kbf-driver-native/src/network.rs`) **[V]**. A VM lease uses the same property,
so a client sets nothing new.

| `network` | Guest gets | Status |
|---|---|---|
| absent, `off`, `none` | **no network device at all**, only the virtio socket to `kbf-guest`. No DNS, no loopback-only trick: the device is not in the configuration. | the default |
| `on`, `standard` | one `VZVirtioNetworkDeviceConfiguration` with a `VZNATNetworkDeviceAttachment`, which does network address translation through the host and "doesn't require your app to have the com.apple.vm.networking entitlement" ([VZNATNetworkDeviceAttachment](https://developer.apple.com/documentation/virtualization/vznatnetworkdeviceattachment)) **[V]** | **refused** for a VM lease, with `INVALID_ARGUMENT` naming the reason, until the LAN-deny design below is decided |

Bridged networking is never used: it needs the `com.apple.vm.networking` entitlement
([VZBridgedNetworkDeviceAttachment](https://developer.apple.com/documentation/virtualization/vzbridgednetworkdeviceattachment))
**[V]**, which Apple grants only on request **[A]**, and it would put the guest on the
farm's network as its own machine.

Why network-on waits: a NAT guest reaches whatever the host reaches, the host itself
and the rest of the farm's network included **[A]**. Keeping a guest off the farm's
own services needs packet-filter rules on the host, which need root **[A]**, and
guest-to-guest isolation is not designed either. The planned egress cache (draft PR
[#192](https://github.com/komira-ai/komira-build-farm/pull/192)) is the natural way out
for a UI test that needs a backend; network-on VM leases are designed with it, not
before. Probe P3 records what a network-off guest has (expected: `lo0` only) and what a
NAT guest can reach.

## 5. Image identity

### 5.1 The recipe digest is what clients route on

An image cannot be named by its bytes alone: the bytes `VZMacOSInstaller` writes are
not reproducible from one build to the next **[A]** (new volume UUIDs and timestamps
at the least), and if every Mac builds its own image (section 5.4), the same inputs
give a different disk on every node. A client must be able to ask for "this macOS,
this Xcode, these runtimes" and land on any node that has them. So an image has two
names:

- **The recipe digest** is the SHA-256 of a canonical recipe file that pins every
  input: the recipe format version; the restore image's URL and SHA-256; the `.xip`'s
  SHA-256; each simulator runtime's build version and the SHA-256 of what was imported;
  the Metal toolchain component's SHA-256; the `kbf-guest` binary's digest and the
  attested CI build that produced it; the first-boot script's digest; the display size
  and the simulator devices to create. Two images built from one recipe are the same
  image to a client.
- `vm.image` carries the recipe digest: `<name>@sha256:<recipe digest>`, matched by
  membership on the digest, as [macos-vms.md](macos-vms.md#51-platform-properties-planned)
  plans. The recipe digest is in the action's platform and therefore in its action
  digest, so a result from one node's build of a recipe is a cache hit for another
  node's build of the same recipe. That is the deliberate trade, the same one the farm
  makes for two hosts with the same Xcode build.

### 5.2 The content digest attests the bytes

Each node keeps, next to each image, a **manifest**: every file of the image directory
with its size and SHA-256, sorted by path. Its SHA-256 is the **content digest**. With
it the node keeps a provenance record: the recipe digest, the content digest, and the
result line of every setup step from the `setup-done` marker.

- The daemon reports `vm.image` for a recipe only after the manifest re-verifies
  against the files on disk (at start, and after any change to the image directory).
  An image whose manifest does not match is not reported, and the reason is shown with
  the fix ("rebuild the image").
- Every VM lease's result carries the content digest of the image it booted, in
  `ExecutedActionMetadata.auxiliary_metadata`, so any result can be traced to the exact
  bytes it ran on even though it was routed on the recipe.

### 5.3 Where this changes the existing design

`macos-vms.md` section 7 step 4 ("the SHA-256 of a manifest of the image's files names
the image") becomes the content digest, and `vm.image` becomes the recipe digest. The
`vm.image` entries planned in [capabilities.md](capabilities.md#planned-vm-entries) carry
recipe digests.

### 5.4 Distribution and the licences

| Option | What moves between Macs | Licence position (no legal conclusion is drawn here) |
|---|---|---|
| **Build on each Mac (lean)** | nothing but the recipe; each Mac needs the `.xip` and the restore image locally | the `.xip` reaches each Mac the way [fleet-updates.md](fleet-updates.md#74-xcode-and-the-metal-toolchain) section 7.4 already plans for bare-metal Xcode, so it adds no new question about the `.xip`. Whether golden images N and N-1 plus up to two running clones fit the macOS licence's limit on virtual copies per Mac is a question for the same legal review; macos-vms.md section 2.2 enforces a count of running guests **[A]** |
| Build once, ship by digest | a built image (macOS, Xcode, runtimes) through the CAS or an object store | the Xcode licence says "You agree not to rent, lease, lend, upload to or host on any website or server ... the Apple Software", quoted in fleet-updates.md section 7.4 **[V]**. Holding an image with Xcode in it on a farm store sits against that sentence just as a `.xip` mirror does. Needs a legal review first |

The lean is to build on each Mac, and so to route on the recipe digest. Whether images
may ever be shipped is an open decision for the project's owner after that review
([section 10](#10-open-decisions)); this document does not settle it.

An image cannot be built in CI on GitHub's hosted macOS runners: they are VMs, and a
macOS guest cannot run a macOS VM (no nested virtualization for macOS guests,
macos-vms.md section 4.1) **[V]**. Images are built on farm Macs.

Cost of building per node: one image build per Mac per recipe, about an hour each
**[A]**, and the disk of macos-vms.md section 7 (100-140 GB per image **[A]**). A guest
must not run a newer macOS than its host **[A]**; the builder refuses a recipe whose
restore image is newer than the host, and probe P7 checks that.

## 6. The run-as uid and the control channel

### 6.1 One throwaway user per VM

`kbf-vmm` must not run as the daemon's role account (macos-vms.md section 6). On `main`,
`kbf-mac-session` serves `user-create`, `run`, `kill-uid` and `user-delete`
(`crates/kbf-mac-session/src/proto.rs`) **[V]**, and `run` starts a process as the
lease's user. So each VM lease gets a lease user of its own:

1. `user-create` for the lease; 2. `run` of `kbf-vmm` as that user, with the lease
directory as its working directory; 3. at the end, `kill-uid`, then the driver
deletes the lease directory (macos-vms.md section 6, step 6), then `user-delete`.

On `main`, `user-delete`'s sweep covers the user's schedules (`crontab`, `at` jobs),
the home folder and the places `SweepPlan::macos()` lists: `/Users/Shared`,
`/private/tmp`, `/private/var/tmp`, `/private/var/folders` and two per-uid launchd
plists (`crates/kbf-mac-session/src/sweep.rs`) **[V]**. It does not cover the daemon's lease
directories, so it does not remove the clone; the driver must.

Files:

- Golden images live in a directory owned by root, files mode 0644 and directories
  0755, so every lease user can read them and none can change them.
- The lease directory is the daemon's. Before `run`, the driver (its owner, so no
  root is needed) adds two inherited ACL entries to it: the lease user may add files
  and subdirectories, and the daemon's account may read and delete everything below,
  the entry fleet-updates-security.md section S4.2 already plans for every Mac lease.
  This is planned; nothing on `main` sets an ACL on a lease directory today.
- `kbf-vmm` makes the clone itself, as the lease user, inside the lease directory:
  `mkdir` for the bundle's directories (which inherit both entries) and `clonefile` on
  each file. The clone is then the lease user's, on the same APFS volume as the golden
  image (macos-vms.md section 4.3).
- Removal: the driver deletes the lease directory, clone included, as the daemon's
  account, through the inherited delete entry. Whether a `clonefile` result takes the
  directory's inherited entry or copies the golden file's (empty) ACL is **[A]**; it
  does not matter for deletion, which needs the delete-child right on the parent
  directory, and the parents are directories `kbf-vmm` made. P5 checks it: after the
  lease, the daemon's account removes the whole directory and nothing the lease user
  made is left. If it cannot, the fallback is a change to the root helper:
  `user-delete` also sweeps the lease directory it is given.
- Disk: the driver checks free space for the clone's growth (about 40 GiB **[A]**)
  before `user-create`.

Whether a VM runs for a uid that has no login session is probe P5 (section 8).

### 6.2 The control channel

`run` passes exactly four descriptors: stdin, stdout, stderr and the lease directory
(`crates/kbf-mac-session/src/helper.rs`) **[V]**. The virtio socket to the guest
(`VZVirtioSocketDevice`, with `connect(toPort:)`,
[VZVirtioSocketDevice](https://developer.apple.com/documentation/virtualization/vzvirtiosocketdevice))
**[V]** lives inside `kbf-vmm`, a different uid from the daemon. Two options:

| Option | How | Verdict |
|---|---|---|
| **A. Relay over stdin and stdout (lean)** | The driver and `kbf-vmm` speak a framed protocol over `kbf-vmm`'s stdin and stdout; `kbf-vmm` relays the guest part of it to `kbf-guest` over the virtio socket; `kbf-vmm`'s stderr is its log. | Lean: no change to `kbf-mac-session`. End of stream on stdout tells the driver the helper has gone. |
| B. A fifth descriptor | `run` passes a socket as a fifth descriptor. | A change to the root helper's protocol for no gain over A. |

The guest agent trusts only a peer that knows the per-boot token `kbf-vmm` writes into
the read-only share before boot, so a process inside the guest cannot pose as the host
on the socket. `kbf-guest` takes one `Run` per boot and refuses a second.

## 7. Signing with the virtualization entitlement

A process that uses Virtualization.framework needs the
`com.apple.security.virtualization` entitlement, "A Boolean value that indicates
whether your app can use the Virtualization framework"
([entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.virtualization))
**[V]**. On `main`, `tools/ci/sign-darwin.sh` signs ad hoc with the hardened runtime,
requires the code directory flags to be exactly `adhoc,runtime`, and **fails any
binary whose signature carries entitlements** **[V]**.

The plan:

- A per-binary allow list in the repository: `kbf-vmm` gets exactly
  `com.apple.security.virtualization`; every other binary gets none, as today.
- `sign-darwin.sh` signs with `--entitlements` only for a binary on the list, and its
  check compares the signature's entitlements to the list's set exactly: one more key,
  one fewer, or any entitlement on an unlisted binary fails. A new check in
  `check-darwin-asset.sh` makes the same comparison on the packaged bytes; the script
  compares no entitlements today.
- Planted defects the later PR must show red: an allow list that lets any entitlement
  through; `kbf-daemon` signed with the virtualization entitlement; `kbf-vmm` with one
  extra key.

Whether macOS accepts an **ad hoc** signature with this entitlement under the hardened
runtime is probe P6 **[A]**. If it does not, `kbf-vmm` needs a signing identity, which
this repository's CI does not have today; that becomes an open decision.

## 8. The launch-daemon risk

In production the chain is: `kbf-daemon` as a launch daemon under its role account,
with nobody logged in on the host, asks `kbf-mac-session` (root) to `run` `kbf-vmm` as a
lease user, which starts the VM. Whether Virtualization.framework starts a macOS guest
in that chain is not known **[A]** (macos-vms.md section 6). Probe P5 tests exactly that
chain, and nothing short of it answers the question:

- A guest booted from a logged-in user's session (probe P1) proves the plumbing only.
- P5 needs `kbf-mac-session` installed as a launch daemon on one Mac, which needs root
  on that Mac. No end-to-end proof of a VM lease in its production shape is possible
  without that one install.
- If P5 fails, the fallback is the open decision macos-vms.md section 10 already names:
  a held session of a dedicated non-admin VM user, as an exception to "auto-login off
  at rest".

The node report (macos-vms.md section 5.2) lists `vm` in `drivers` only when a check
passes. The lean is a check that boots nothing, because a boot takes 20-60 s **[A]**
and holds one of the two slots: `kbf-vmm probe`, run through the same `run` chain,
prints whether `VZVirtualMachine.isSupported` is true, the entitlement is present, the
`cpuCount` and `memorySize` bounds, free disk, and which images re-verify. A failing
check keeps `vm` out of `drivers`, shows the failing item with its fix in the node's
status, raises it as an alert item, and returns `vm` by itself once the check passes
again, with no restart. The first real boot of every lease still proves the rest.

## 9. Not in a VM: physical devices

Virtualization.framework does not pass a host's USB devices through to a guest **[A]**
(it does emulate USB mass storage for a guest, which is not the same thing). So tests on
a physical iPhone or iPad never run in a VM lease. They need their own host-side design
(pairing, Developer Mode, a signing team), which this document does not cover. VM
leases cover simulators and macOS GUI and UI tests.

## 10. Open decisions

| Decision | Options | Lean |
|---|---|---|
| Getting past Setup Assistant | keystrokes over VNC; MDM; everything offline; first-boot daemon | first-boot daemon (section 2.2) |
| Who writes the two offline files, if P0 fails | a narrow root helper verb on the host; the MDM | decided only if P0 fails |
| Accepting the Xcode licence inside an automated image build | a standing ruling for guest images; a person runs and records it per recipe | an owner decision; not automated until it is made |
| Shipping built images between Macs | never; through the CAS after a legal review | build on each Mac until a legal review says otherwise (section 5.4) |
| Network for VM leases | off only; NAT with a LAN-deny design and the egress cache | off only for the first version (section 4) |
| Control channel to `kbf-vmm` | relay over stdin/stdout; a fifth descriptor | relay (section 6.2) |
| Screen Recording grant in the guest | none needed; written offline; capture not offered | P9 decides (section 2.4) |
| If ad hoc signing with the entitlement is refused | a signing identity in CI; no VMs | decided only if P6 fails |

## 11. Probes later PRs must run

None of these can run on GitHub's hosted macOS runners, which cannot boot a macOS guest
(section 5.4). Each runs on one farm Mac, needs an explicit go for that Mac and that
probe, and is recorded with its command, its output and the host's macOS and Xcode
builds. None needs a person at the console. Hosted runners do prove what needs no
guest: `kbf-guest` as a LaunchAgent in the runner's own GUI session, the signing allow
list, and, if a run shows it, `isSupported` being false inside a VM (a real negative)
**[A]** until a hosted run prints it.

| Probe | Question | Shape | Root on the host |
|---|---|---|---|
| P0 | Can a host user without root write a root-owned file into an attached guest volume? | attach a scratch disk image, write one plist with owner root, detach, check the owner from a second attach | no (that is the question) |
| P1 | Does a golden clone boot and `kbf-guest` answer over the virtio socket? **Plumbing only**: it runs from a logged-in user's session, not the production chain | boot one clone, one `Run` of `/usr/bin/true`, destroy | no; needs an image, so after P8 |
| P2 | Does a third running guest fail with `VZErrorVirtualMachineLimitExceeded`, and does a `kbf-vmm` killed with `SIGKILL` free its slot? | boot two, try a third; kill one helper with -9, boot again within a bound | no |
| P3 | Does a network-off guest have no interface but `lo0`, and what can a NAT guest reach? | `ifconfig -l` in each; a NAT guest's reach to the host and the farm network recorded | no |
| P4 | Boot time, idle guest memory, clone growth over one XCUITest run, virtiofs throughput for a build step | timed boots, `vm_stat` in the guest, the clone's allocated size | no |
| P5 | Does a VM start from a launch daemon's `kbf-mac-session run` child, as a lease user, with nobody logged in? | the production chain of section 8; after the lease, the daemon's account removes the lease directory, clone included, and nothing the lease user made is left (section 6.1) | **yes**: `kbf-mac-session` installed as a launch daemon |
| P6 | Does macOS accept an ad hoc, hardened-runtime signature carrying the virtualization entitlement? | start a VM from a binary signed the way section 7 plans | no |
| P7 | Is a guest newer than its host refused, and is that refusal what the builder checks? | the builder's check against the host's build; one attempted newer restore image | no |
| P8 | Does a full image build with the first-boot daemon (section 2.3) pass its verification boot? | `kbf-vmm image build` of one recipe | only if P0 failed |
| P9 | Do XCTest screenshots, `simctl io` and `screencapture` in the guest need a Screen Recording grant, and does an offline-written grant work? | each capture in a guest from P8, with and without the row | no |

## 12. Verified and assumed

| Claim | Status | Source |
|---|---|---|
| Cirrus clicks through Setup Assistant with timed keystrokes and sets auto-login with `kcpassword` | V | [vanilla-tahoe.pkr.hcl](https://github.com/cirruslabs/macos-image-templates/blob/main/templates/vanilla-tahoe.pkr.hcl) |
| Virtualization.framework has no public VNC server | A | |
| The host sees a guest's screen only through `VZVirtualMachineView`, an `NSView` | V | [VZVirtualMachineView](https://developer.apple.com/documentation/virtualization/vzvirtualmachineview) |
| A VM has no serial for automated MDM enrolment | A | |
| `automationmodetool` needs an administrator | V | [automationmodetool(1)](https://keith.github.io/xcode-man-pages/automationmodetool.1.html) |
| Without `DevToolsSecurity` and `_developer`, a test host can raise an administrator prompt | A | |
| The guest TCC database is SIP-protected while it runs | A | |
| `VZMacOSInstaller` output is not byte-reproducible | A | |
| NAT needs no entitlement; bridged needs `com.apple.vm.networking` | V | [NAT](https://developer.apple.com/documentation/virtualization/vznatnetworkdeviceattachment), [bridged](https://developer.apple.com/documentation/virtualization/vzbridgednetworkdeviceattachment) |
| A NAT guest reaches the host's networks | A | P3 |
| A shared directory can be read-only to the guest | V | [VZSharedDirectory](https://developer.apple.com/documentation/virtualization/vzshareddirectory) |
| The virtio socket connects host to guest ports | V | [VZVirtioSocketDevice](https://developer.apple.com/documentation/virtualization/vzvirtiosocketdevice) |
| The virtualization entitlement is required | V | [entitlement](https://developer.apple.com/documentation/bundleresources/entitlements/com.apple.security.virtualization) |
| Ad hoc signing with the entitlement works under the hardened runtime | A | P6 |
| `sign-darwin.sh` refuses entitlements; `run` passes four descriptors; the `network` property exists | V | `tools/ci/sign-darwin.sh`, `crates/kbf-mac-session/src/helper.rs`, `crates/kbf-driver-native/src/network.rs` |
| A guest cannot run a newer macOS than its host | A | P7 |
| `user-delete`'s sweep does not cover the daemon's lease directories | V | `crates/kbf-mac-session/src/sweep.rs` (`SweepPlan::macos()`) |
| The daemon's account can remove a lease directory holding the lease user's clone through inherited ACL entries | A | P5 |
| Hosted macOS runners report `isSupported` false | A | a hosted run |
| No USB passthrough of host devices to a guest | A | |
