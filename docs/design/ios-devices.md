# Physical iOS devices on Mac nodes

This document says how kbf will run work on real iPhones and iPads plugged into a Mac
node by USB: how a device is set up once, how signing is done and where its secrets
live, how one specific device is booked for one lease, how the daemon reports each
device's health and alerts with the exact fix, how a device is cleaned between
leases, and what still needs a person at the device. **Everything in it is planned.**
Nothing about physical devices exists on `main`: no daemon reports a device, no
request key names one, and no runtime talks to one. What exists on `main` today is
named as such.

Claims about Apple's software are marked **[V]** (read in the source linked) or
**[A]** (an assumption nobody has checked on our hardware). Most claims about
`devicectl` are **[A]**: its exact verbs, its JSON and its behaviour without a
logged-in user are what the probes in [section 11](#11-probes) settle.
[Section 14](#14-verified-and-assumed) lists them together.

## 1. Summary

| Question | Answer (planned) |
|---|---|
| Where device work runs | On the Mac the device is plugged into, as an `action` lease on bare metal. Never in a VM, and it does not take the whole Mac. |
| How the host talks to the device | `xcrun devicectl` (Xcode's CoreDevice command line) for the daemon's own checks and cleanup; `xcodebuild test-without-building -destination id=<udid>` inside the action. |
| How a device is booked | As a named object, not a count. Each ready device is one `ios.device` report entry. An action asks for `ios.device=1` plus exact attributes; one free device must satisfy all of them. The scheduler books that device's id for the lease, and `Start` names it. |
| How the action learns its device | `KBF_IOS_DEVICE_ID=<udid>` in its environment; the action is a wrapper script that passes it to `xcodebuild`. |
| Signing | Not on the farm in the first version: the client sends bundles signed with a development profile that lists the farm's devices. Where an on-farm signing identity would live is an open decision ([section 4](#4-signing-provisioning-profiles-and-secrets)). |
| Health | A daemon watch surveys every device, reports every one of them (ready or not) in `NodeStatus` and `GET /v1/nodes`, raises an attention item with the exact fix for each one that is not ready, and advertises it again on its own once it is fixed. A device is never dropped silently. |
| Between leases | Uninstall every app the lease added, check the device is ready again, and only then free it. A failed cleanup quarantines the device with an alert and one automatic reboot. No full erase per lease. |
| What needs a person | Plugging in, tapping Trust, turning Developer Mode on, unlocking a device that has a passcode after it restarts, and any erase, until devices are supervised through MDM ([section 10](#10-what-needs-a-person-at-the-device)). |

## 2. Where device work runs

**On the host, never in a VM.** Virtualization.framework attaches USB mass-storage
devices to a guest (macOS 15 and later), not arbitrary host USB devices such as an
iPhone **[A]**, so a guest cannot see a plugged-in device. A device lease therefore
runs on bare metal under the native driver, beside the node's other `action` leases.
It uses no VM slot, so it does not count against the licence's two guests per Mac
([macos-vms.md](macos-vms.md#22-the-licence-two-guests-per-mac)).

**Not a whole-machine lease.** The test's UI runs on the device, not on the Mac, so in
principle the host needs no window server **[A]**: probe P-D3 settles whether
`xcodebuild` can drive XCUITest on a device from the daemon's context with nobody
logged in. A test that drives a macOS GUI as well as a device is out of scope.

**A simulator is not a device.** Simulator and XCUITest-on-simulator work stays in VM
leases ([macos-vms.md](macos-vms.md#32-do-swift-and-xcode-builds-need-a-vm)); this
document is only about physical hardware.

## 3. Setting a device up

A device joins the farm once, by hand, until supervision removes most of these steps
([section 12](#12-phased-plan), phase D4):

| Step | Who | Why |
|---|---|---|
| Plug the device into a powered USB hub on the node | a person | the host reaches the device over USB; a device reached only over Wi-Fi is not used ([section 7](#7-health-report-alert-recover)) |
| Unlock it and tap **Trust** on the device | a person at the device | pairing creates the host's pairing record **[A]**: whether it is per macOS user or system-wide is probe P-D1 |
| Turn on **Developer Mode** (Settings > Privacy & Security > Developer Mode, restart, confirm) | a person at the device | iOS 16 and later run development-signed apps only with it on **[A]** |
| Set Auto-Lock to Never and no passcode, or accept that every restart needs an unlock | a person at the device | a locked device cannot run a test; a device with a passcode stays locked after every restart until someone unlocks it **[A]** |
| Turn off automatic iOS updates | a person at the device | an update restarts the device and changes its iOS version, and so which requests it matches, without notice **[A]** |
| Register the device's UDID in the Apple Developer account and add it to the development profile | whoever holds the account | a development profile must list every device it installs on **[A]**; the farm publishes UDIDs in `GET /v1/nodes` for this |
| Accept the licence of every Xcode on the node | the node's administrator | `devicectl` and `xcodebuild` need it; the Xcode attention item of PR #173 already names that fix, and device states point to it rather than repeat it |

Developer Mode, pairing and a passcode-free setup each make a device easier to use for
anyone holding it. The farm's devices are test hardware: no personal account, no
Apple ID signed in unless a test needs one.

## 4. Signing, provisioning profiles and secrets

An app runs on a device only if it is signed with a certificate whose development
profile lists that device **[A]**. In the first version **the farm does not sign**:

- The client sends the app, its `-Runner.app` and an `.xctestrun`, already signed with
  a development profile that lists the farm's devices.
- The runtime maps `devicectl`'s or `xcodebuild`'s "profile does not include this
  device" failure to an action error that names the device's UDID, not to a farm
  error: retrying elsewhere cannot fix it, and a farm error would retry it.
- Adding a device means every bundle must be signed again with a profile that lists
  it. Device registrations are counted per product family per membership year
  **[A]**, and removing a device does not free its slot before the year ends **[A]**.

Signing has to happen somewhere, and every option needs the signing identity (a
`.p12` and its password) and, for automatic device registration, an App Store
Connect API key. kbf has no secret store today. The options, none chosen:

| Where signing runs | Where the identity lives | Notes |
|---|---|---|
| A developer's own Mac, outside the farm | that Mac's login keychain | no secret on the farm; does not fit a workflow where every build goes through the farm |
| On Linux in a farm action, with `rcodesign` (apple-codesign) | a secret the front hands one lease, from a store kbf would have to add | no Mac needed; the secret reaches an action's process, so the lane must be separate from ordinary work |
| A dedicated signing lane on a Mac | a per-lease keychain built from the store, with `security set-key-partition-list`, removed after the lease | `codesign` from a non-GUI session fails with `errSecInternalComponent` ([macos-vms.md](macos-vms.md#32-do-swift-and-xcode-builds-need-a-vm)), so this lane needs the session work of `kbf-mac-session` |
| An operator-held key and a held signing step | an operator's machine | no secret on the farm; every new device or profile waits for that person |

Whatever is chosen, a secret never goes in a platform property (properties are part of
the action digest and are logged) and never in the repository.

## 5. Scheduling: one specific device per lease

### 5.1 Why a device is not a count

`gpu` is a count ([platform-properties.md](../platform-properties.md#gpu)): GPUs on one
node are interchangeable. Devices are not: two iPhones on one node can differ in model
and iOS version, and a request for "an iPhone on iOS 26.0" must be met by **one**
device that is both. Matching today is per node over flat `(key, value)` entries
([capabilities.md](capabilities.md#matching)), with no way to say "one device that has
both". And the daemon never learns which GPU, or even that one was booked: `Start`
carries only CPU and memory, and the daemon builds `Work.resources` from those two
(`crates/kbf-daemon/src/lease.rs`). Telling the daemon which sub-node resource it holds
is new.

### 5.2 Report entries

One `ios.device` report entry per **ready** device (repeated). Its value is a sorted,
comma-separated `k=v` list:

| Field | Example | Meaning |
|---|---|---|
| `id` | the UDID | the device; what `Start` and `KBF_IOS_DEVICE_ID` name |
| `class` | `iPhone`, `iPad` | the device class |
| `product_type` | `iPhone17,3` | Apple's model identifier |
| `os_version` | `26.0` | iOS version |
| `os_build` | `23A341` | iOS build |

A device that is not ready is not in the report, so no work routes to it; it is in
`NodeStatus` ([section 7](#7-health-report-alert-recover)). The `Hello` wire is
unchanged: a server that does not know `ios.device` skips the entry, as it skips every
report entry it does not know ([capabilities.md](capabilities.md#matching)).

### 5.3 Request keys

| Key | Value | Comparison |
|---|---|---|
| `ios.device` | `1` | books one device; any other value is refused |
| `ios.device.class` | `iPhone`, `iPad` | exact |
| `ios.device.product_type` | a model identifier | exact |
| `ios.device.os_version` | an iOS version | exact |
| `ios.device.os_build` | an iOS build | exact |

A node matches only if **one free device satisfies every `ios.device.*` key**. An
attribute key without `ios.device=1` is refused, so is an `ios.device.*` key kbf does
not know, and so is `ios.device` together with `os=linux`, `kbf-lease=vm` (no USB in a
guest) or `kbf-lease=whole_machine` (not in the first version). Names are read in any
case, like every kbf key. When no node has such a device, the request waits and then
fails as any unservable request does (`--unservable-wait-secs`), and the reason names
the closest device and the key it fails.

### 5.4 Booking

- The scheduler keeps, per worker, the ready device ids from its report and the ids
  booked by its leases.
- Placement chooses a free device that satisfies the request and books it together
  with the lease's CPU and memory.
- The device is freed when the lease ends: its result is accepted, the lease is given
  up, or it is fenced. A resent `Hello` that no longer lists a booked device keeps the
  booking until the lease ends.
- Until probe P-D2 shows that leases cannot reach each other's devices, a node runs at
  most one device lease at a time ([section 6.1](#61-what-the-sandbox-must-allow)).
- `Start` gains `device_id` (field 8, the next free number), and the daemon's `Work`
  gains `device: Option<String>`.
- A device lease books one core and 1 GiB by default, like every action. Running
  `xcodebuild test-without-building` needs more **[A]**, so device tests are expected to
  name `kbf-book-cpus` and `kbf-book-mem-gib`.

### 5.5 Rollout order

Two facts on `main` decide the order in which these pieces may ship:

- **An unknown platform property is ignored today**
  ([capabilities.md](capabilities.md#unknown-keys)), so `ios.device=1` sent to a server
  that does not know it matches every node, Linux included. The server must know
  `ios.device` (and refuse it until booking exists) before any client sends it.
- **An old daemon would drop `device_id` silently.** `Start` is proto3; a daemon that
  predates field 8 decodes the message without it and would run the action without
  knowing its device. The guard is the report: only a daemon that reports
  `ios.device` entries can match a device request, and an old daemon never reports
  one. The test for this is "a node whose report has no `ios.device` entry is never
  placed a device lease".

So: matching and booking land together (or matching lands with the front refusing
`ios.device` until booking is in), then the server, then daemons.

### 5.6 Caching

Platform properties are part of the action digest, so `ios.device.os_version` splits
the action cache by iOS version, which is correct. Whether a passing device test may
be served from the action cache like any other test, or must always run, is an open
decision ([section 13](#13-open-decisions)).

## 6. The action contract

The scheduler, not the client, picks the device, so the UDID cannot be in the
action's command line (the command is part of the digest and fixed before
placement). The runtime passes it in the environment:

- `KBF_IOS_DEVICE_ID=<udid>` is set by the platform for the lease, the same pattern as
  `CUDA_VISIBLE_DEVICES`. It is not part of the digest.
- The action is a wrapper script that expands it:

```sh
#!/bin/sh
# run_on_device.sh: the inputs hold the signed .xctestrun and its bundles
exec xcodebuild test-without-building \
  -xctestrun "$1" \
  -destination "id=${KBF_IOS_DEVICE_ID:?no device booked}" \
  -derivedDataPath "$PWD/dd" \
  -resultBundlePath "$PWD/out/result.xcresult"
```

```starlark
# Bazel: a test that runs on one iPhone with iOS 26.0
sh_test(
    name = "app_ui_test_device",
    srcs = ["run_on_device.sh"],
    args = ["$(location :app_ui_test.xctestrun)"],
    data = [":app_ui_test.xctestrun", ":app_ui_test_bundles"],
    exec_properties = {
        "OSFamily": "Darwin",
        "xcode": "17A324",
        "network": "on",
        "ios.device": "1",
        "ios.device.class": "iPhone",
        "ios.device.os_version": "26.0",
        "kbf-book-cpus": "4",
        "kbf-book-mem-gib": "8",
    },
)
```

Buck2's and Bazel's Apple test rules do not do this today; they name a simulator or
expect the client to know the device. A device test needs a rule like the one above.
The `xcode` and `ios.device.*` values above are examples.

### 6.1 What the sandbox must allow

Every action on a Mac runs under `sandbox-exec`
([platform-properties.md](../platform-properties.md#what-an-action-on-a-mac-may-write)).
A device action needs, all **[A]** until probe P-D3:

- **The network.** CoreDevice talks to the device over a tunnel on a non-loopback
  interface **[A]**; the no-network profile on `main` allows only loopback and Unix
  sockets (`crates/kbf-driver-native/src/network.rs`). So a device action needs
  `network=on`, or a narrower rule for the tunnel that P-D3 finds.
- **File writes outside the lease.** Xcode keeps device symbols under
  `~/Library/Developer/Xcode/iOS DeviceSupport` (copied when a device or an iOS
  version is first seen, often gigabytes) and CoreDevice state under
  `~/Library/Developer/CoreDevice` **[A]**. The sandbox denies writes outside the lease
  and moves `HOME` into it, so every lease may redo "Preparing device", or fail.
  `xcodebuild` finds `~` through the user database, not `HOME`
  ([platform-properties.md](../platform-properties.md#what-an-action-on-a-mac-may-write)),
  so P-D3 must say which of these paths it writes and whether a narrow allow rule, a
  daemon-made cache or a per-lease copy is the fix.
- **Preference writes.** The profile denies every preference write; whether
  `xcodebuild` on a device needs one is part of P-D3.

The daemon itself runs `devicectl` for its survey, precheck and cleanup
([section 8](#8-the-lease-lifecycle)) under the actions' sandbox too, as it asks its
Xcodes every question (CEO decision on issue #164: `xcrun` reads and fills a cache
that leases can write); its profile is that of a device lease, which may reach
CoreDevice (below). The action runs `xcodebuild` inside it.

**Other leases must not reach a booked device.** The profile on `main` allows mach and
XPC services (`(allow default)`) and every Unix socket, so any concurrent action on the
node, device lease or not, could run `xcrun devicectl` and install, launch or reboot
apps on a device booked for another lease. The planned fix: the profiles of leases
without a device deny the CoreDevice mach services and the `usbmuxd` socket. That
part needs no device and can be tested on the hosted macOS runner: `devicectl list
devices` from a lease without a device must be refused. Two device leases on one
node still share these services; until P-D2 shows a device per user, a node may run
one device lease at a time.

## 7. Health: report, alert, recover

Every failure mode is handled in three parts, the model PR #173 sets for Xcodes:
**report** it, **alert** on it with the exact fix, and **recover** by itself once the
cause is fixed. Work is kept off a device only alongside the report and the alert.

### 7.1 The survey

A daemon watch asks, every `--device-recheck-secs` and on each USB attach or detach:

1. `xcrun devicectl list devices --json-output <file>`: every device the host knows,
   with its transport (USB or network) **[A]**;
2. for each device, `device info details`: class, model, iOS version and build, pairing
   and Developer Mode state, battery level if the JSON carries it **[A]**;
3. `device info lockState`: locked or not **[A]**.

Each question has a time limit (one minute), and an unknown JSON field is ignored, so a
newer Xcode does not break every node. The first question that fails decides the
device's state.

### 7.2 States and fixes

| State | Meaning | Fix shown in the alert | Recovers by itself | Needs a person at the device |
|---|---|---|---|---|
| `ready` | paired, Developer Mode on, unlocked, wired, battery above the floor, its iOS supported by a ready Xcode | - | - | - |
| `not_paired` | plugged in, Trust not tapped | `at <node>, unlock <name> and tap Trust` | yes, within one recheck of the tap | yes |
| `developer_mode_off` | paired, Developer Mode off | `on <name>: Settings > Privacy & Security > Developer Mode, restart, confirm` | yes, once on | yes |
| `locked` | a passcode is set and the device is locked, or not yet unlocked since it restarted | `unlock <name> at <node>; set Auto-Lock to Never` | yes, once unlocked | yes |
| `os_unsupported` | its iOS is newer than every ready Xcode on the node supports | `install an Xcode that supports iOS <version>` | yes, once that Xcode is ready | no |
| `preparing` | the developer disk image (DDI) is being mounted, or symbols copied | none: wait | yes | no |
| `ddi_failed` | the DDI did not mount | `allow the node to reach Apple's DDI signing service`, or `failed` with the reason | yes, retried each recheck | no |
| `network_only` | reached over Wi-Fi only | `plug <name> into <node> by USB` | yes, once wired | yes |
| `low_battery` | below the floor while plugged in: a cable, hub or power fault | `check the cable and hub port of <name>` | yes, once above the floor | yes |
| `cleanup_failed` | the last lease's cleanup did not pass ([section 8](#8-the-lease-lifecycle)) | `see the lease <id>; run kbf-node device release <id> once checked` | one automatic reboot first | sometimes |
| `gone` | seen before, now missing | `plug <name> back into <node>, or kbf-node device forget <id>` | yes, once it is back | yes |
| `failed` | a question failed for another reason | the reason, no fix known | retried each recheck | maybe |

An Xcode that is not ready (licence not accepted, first launch not run) is PR #173's
attention item. When the only Xcode that would support a device's iOS is installed
but not ready, the device's `os_unsupported` line names that Xcode's item instead of
a second fix. Accepting a licence is never automated.

### 7.3 Report

- `NodeStatus` gains `devices`: id, name, class, product type, iOS version and build,
  transport, state, reason, fix, battery percentage, last seen. Every device the
  daemon knows is listed, whatever its state.
- `GET /v1/nodes` shows them under `software.devices`, and each device that is not
  ready adds a `needs_attention` line:
  `iOS device <name> (<id>) on <node>: <state>: <reason>; fix: <fix>`.
- The report carries only `ready` devices as `ios.device` entries
  ([section 5.2](#52-report-entries)).
- The farm's UDIDs are in `/v1/nodes` so they can be registered
  ([section 4](#4-signing-provisioning-profiles-and-secrets)).

### 7.4 Alert

As PR #173 does for Xcodes: the server logs each attention item once when it appears,
at `WARN` under `kbf_server::attention`, and once at `INFO` when it clears; a status that
repeats the same items logs nothing. The daemon logs each state change of a device
once. Delivering items to an operator (webhook, raise and clear, surviving a server
restart) is issue #189.

### 7.5 Recover

- When a survey differs from the last, the daemon resends `NodeStatus`; when the set
  of ready devices changed, it resends `Hello` too (a resent `Hello` is the node's new
  report, issue #25). A device that is fixed is advertised within one recheck, with no
  restart.
- **A device is never dropped silently.** The daemon keeps every device it has seen in
  a state file, written atomically. A device that disappears is `gone`, with an
  attention item, until it returns or an operator runs `kbf-node device forget <id>`.
  A state file that does not parse is itself an attention item; the daemon starts.
- In PR #173 the driver report is one watch channel whose only producer is the Xcode
  watch, and its payload holds the whole report's driver entries. A second producer
  would overwrite the first's entries, and a device's state depends on which Xcodes
  are ready. So the device survey and the Xcode survey become one driver watch, or
  that channel is split into one part per source. If PR #173 changes in review, this
  section follows it.

## 8. The lease lifecycle

On a `Start` with `device_id`, the native runtime:

1. **Prechecks** the device: `ready`, unlocked, above the battery floor, DDI mounted
   for the lease's `xcode`. A device that fails is a farm error, so the lease is
   retried elsewhere; it is never shown to the client as its own fault.
2. **Records a baseline**: the apps installed on the device (`device info apps`)
   **[A]**.
3. **Runs the action** with `KBF_IOS_DEVICE_ID` set ([section 6](#6-the-action-contract)).
4. **Cleans up**: ends the processes the lease launched on the device, uninstalls
   every app not in the baseline, and surveys the device again.
5. **Releases** the device to the scheduler only once cleanup passes. The lease's
   result can be sent before; the booking ends after cleanup.

If cleanup fails, the device goes to `cleanup_failed` with an attention item, and the
runtime reboots it once (`devicectl device reboot` **[A]**) and cleans up again. A pass
releases it and clears the item; a second failure keeps it quarantined until an
operator releases it.

**No full erase per lease.** An erase removes pairing and Developer Mode, which means a
person at the device unless it is supervised. Erase is an operator action, or a later
policy ([section 12](#12-phased-plan)).

What survives between leases, known and accepted for a farm with one tenant:

- keychain items an app stored survive its uninstall **[A]**;
- settings a test changed on the device are not reverted;
- any process on the host can address every attached device (`devicectl` has no
  per-lease view **[A]**), which the sandbox rule of [section 6.1](#61-what-the-sandbox-must-allow)
  narrows to device leases.

A post-lease audit compares each device's app list with its last baseline; an app that
appeared on a device no lease held raises an attention item.

## 9. Failure modes

| Failure | Detected by | Reported as | Automatic recovery | Person needed |
|---|---|---|---|---|
| Unplugged, cable fault | survey; USB detach | `gone` (or `network_only` if still on Wi-Fi) | re-advertised when it is back | to plug it back |
| Untrusted (new device, host's pairing record removed, device erased) | survey | `not_paired` | re-advertised after Trust | to tap Trust |
| Locked | survey; precheck | `locked` | re-advertised once unlocked | to unlock it, if it has a passcode |
| Developer Mode off (after an erase or an iOS reset) | survey | `developer_mode_off` | re-advertised once on | to turn it on |
| Low battery | survey; precheck | `low_battery` | re-advertised above the floor | to fix the cable or port |
| iOS newer than every ready Xcode | survey | `os_unsupported` | re-advertised once a newer Xcode is ready | no (an Xcode rollout) |
| DDI does not mount (no route to Apple's service) | survey | `ddi_failed` | retried each recheck | no (egress) |
| Device restarts during a lease | the action fails; the next survey | the lease's result; then `locked` or `ready` | none for the lease; the device recovers if it has no passcode | if it has a passcode |
| Cleanup fails | cleanup | `cleanup_failed` | one reboot and cleanup again | if the second attempt fails |
| Host restarts | daemon start | each device's state | each device is surveyed again; remembered devices that are missing are `gone` | to unlock devices with a passcode |
| Profile does not list the device | install in the action | the action's error, naming the UDID | none: the client must sign again | no |

## 10. What needs a person at the device

Until devices are supervised:

- plugging a device in, and back in after it is unplugged;
- tapping Trust, once per device and again after an erase or a lost pairing record;
- turning Developer Mode on, once per device and again after an erase;
- unlocking a device that has a passcode, after every restart;
- approving an iOS update, if updates are taken;
- erasing a device;
- clicking Allow on a Local Network privacy prompt on the Mac, if P-D3 shows one: Apple
  exempts launchd daemons from local network privacy but not agents **[A]**.

The alert of [section 7.2](#72-states-and-fixes) names each of these with the exact
step. Supervision (Apple Configurator or Automated Device Enrollment into an MDM) is
expected to remove Trust, the passcode, Auto-Lock and update steps and allow an
unattended erase **[A]**: probe P-D7.

## 11. Probes

None of these has run. Each is a later PR's evidence; the PR that needs it says so.

| Probe | What it settles | Needs |
|---|---|---|
| P-D0 | `xcrun devicectl help` and `--version` text for the pinned Xcode: which verbs and JSON options exist | a Mac with Xcode, no device; can run on the hosted macOS runner |
| P-D1 | Real `devicectl` JSON for `list devices`, `device info details`, `lockState` and `apps`, in each state: paired and ready, Trust not tapped, Developer Mode off, locked, unplugged, Wi-Fi only; whether the JSON has a battery field; whether pairing records are per user | a device and a person at it; fixtures scrubbed of UDID, ECID, serial number, CoreDevice id and device name |
| P-D2 | Whether a per-lease macOS user (`kbf-mac-session`, issue #122) sees and can use a device paired by another user | root on the node; a device |
| P-D3 | The deciding probe: `xcodebuild test-without-building -destination id=<udid>` on a signed XCUITest bundle, from the daemon as a LaunchDaemon and as a LaunchAgent, with someone logged in and with nobody logged in, inside the sandbox with the network off and on, with a per-lease `HOME`; which paths and preferences it writes; whether a Local Network prompt appears | a signed bundle and a profile listing the device; an unlocked device; maybe a person at the Mac's console |
| P-D4 | Install, uninstall and reboot back to `ready` with no person, on a device with no passcode | a device |
| P-D5 | The lifecycle: unplug and replug, host restart, Xcode update, iOS update. Does pairing survive each? Is the DDI mounted again by itself? | a device; a person to replug and approve the update |
| P-D6 | The DDI mount with egress blocked, and which destinations it needs | a device; control of the node's egress |
| P-D7 | A supervised device: pairs without the Trust prompt; Developer Mode turned on without touching it (`devmodectl` **[A]**); erase without a person | an MDM enrollment of the device; a person for the first enrollment |

## 12. Phased plan

All planned; each is its own pull request.

| Phase | What | Needs |
|---|---|---|
| D0 | This design | - |
| D1 | Probe kit: P-D0 and P-D1 fixtures, with a lint that refuses a fixture carrying a real-looking UDID, ECID, serial, CoreDevice id or device name | a device and a person (P-D1) |
| D1 | Sandbox rule: leases without a device cannot reach CoreDevice or `usbmuxd`; tested on the hosted macOS runner | none |
| D2 | Survey parser in `kbf-driver-native`: `devicectl` JSON to device states, tested against the D1 fixtures | D1 |
| D2 | One driver watch for Xcodes and devices (generalizes PR #173's channel) | PR #173 |
| D2 | `NodeStatus.devices`, attention lines, the state file, `forget` and `release`, `Hello` resent on change | the watch |
| D3 | `kbf-caps` device matching (one device satisfies every key) with scheduler booking and `Start.device_id`, landed together, then the front's refusals and messages | none |
| D3 | Device lease runtime: precheck, baseline, run, cleanup, quarantine, audit | D2, D3, P-D3, P-D4 |
| D4 | Supervision through MDM, an erase policy, iOS update control in [fleet-updates.md](fleet-updates.md), a signing lane with a secret store, alert delivery (issue #189) | open decisions below; P-D7 |

## 13. Open decisions

| Decision | Options |
|---|---|
| Which devices, how many, which iOS versions | pinned older versions; always the latest; both |
| Which nodes host devices | ordinary Mac nodes; a separate tier of small Macs for devices |
| Power | an ordinary powered hub; a hub that limits charge (a device on charge all the time risks its battery) |
| Who is the person at the device, and is an alert with the exact fix enough | - |
| Supervision | stay unsupervised (more human steps); supervise through Apple Configurator or Automated Device Enrollment |
| Passcode | none (a restart recovers by itself, but anyone holding the device can use it); a passcode (every restart needs an unlock) |
| Signing | client-signed only (first version); one of the on-farm options of [section 4](#4-signing-provisioning-profiles-and-secrets), and where its secrets live |
| Cleanup | uninstall what the lease added (first version); also erase for some tests (minutes, and re-pairing unless supervised) |
| Caching | serve a passing device test from the action cache; always run device tests |
| Which user runs device leases until P-D2 passes | the daemon's own user; none until per-lease users work with devices |

## 14. Verified and assumed

| Claim | Status | Source |
|---|---|---|
| No physical-device code or report entry exists on `main` | V | the repository |
| `gpu` is booked as a count; `Start` carries only CPU and memory; the daemon builds `Work.resources` from those | V | `crates/kbf-types/src/work.rs`, `crates/kbf-proto/proto/kbf/worker/v1/worker.proto`, `crates/kbf-daemon/src/lease.rs` |
| `Start`'s field numbers 1 to 7 are taken | V | `worker.proto` |
| An unknown platform property is ignored today | V | [capabilities.md](capabilities.md#unknown-keys) |
| The no-network profile allows only loopback and Unix sockets; every profile denies file writes outside the lease and preference writes, and allows mach and XPC | V | `crates/kbf-driver-native/src/network.rs` |
| Virtualization.framework attaches USB mass storage only, not an iPhone | A | to be read in the framework's USB documentation |
| Developer Mode is needed for development-signed apps on iOS 16 and later | A | Apple's "Enabling Developer Mode on a device" (not re-read) |
| The `devicectl` verbs and JSON named here | A | P-D0, P-D1 |
| Pairing records are per user or system-wide | A | P-D1, P-D2 |
| XCUITest on a device works from a launchd daemon with nobody logged in | A | P-D3 |
| CoreDevice uses a tunnel on a non-loopback interface | A | P-D3 |
| Xcode writes DeviceSupport and CoreDevice state under the user's home | A | P-D3 |
| Launchd daemons are exempt from local network privacy; agents are not | A | Apple TN3179 (not re-read here) |
| Mounting the personalized DDI needs Apple's service online | A | P-D6 |
| Keychain items survive an app's uninstall | A | - |
| Device registrations are counted per product family per year, and a removed device's slot is not freed before renewal | A | Apple Developer account help (not re-read) |
| Supervision removes the Trust prompt and allows an unattended erase and Developer Mode | A | P-D7 |
| `codesign` from a non-GUI session fails with `errSecInternalComponent` | V for SSH; A for a launch daemon | [macos-vms.md](macos-vms.md#32-do-swift-and-xcode-builds-need-a-vm) |
