# kbf on macOS: end state

Status: **planned**. The end state of the native and whole-machine drivers on macOS, and the
build order that reaches it. References are to `main` at b80d5443. Labels: **[V-repo]** read
in the repository (path given); **[V-doc]** an Apple document or an open-source file;
**[I]** inferred, not run. Every [I] that a choice rests on has a probe in section 3.

**The goal.** A Mac farm that does not break: one lease cannot get in another lease's way,
and cannot leave anything behind that breaks the next lease. The goal is not a jail. Actions
are our own builds and tests. Linux runs them in rootless containers ([daemon.md](daemon.md));
this document is about Macs only.

## 1. The end state in one table

| Piece | End state | Why (reliability) |
|---|---|---|
| **Daemon** | `kbf-daemon` runs as **root**, as a system LaunchDaemon with `KeepAlive`. The `UserName`/`GroupName` keys leave the plist (`kbf-mac-provision/src/launchd.rs:79` sets them today). The `_kbf` role user is retired. The node key is `root:wheel 0600`. The daemon is signed with the farm identity (D5), so a Full Disk Access grant, if U3 needs one, survives every update. | Only root can switch an action to another uid, kill a uid's processes and wipe a uid's files. One root process, with no helper to keep in step with it. A TCC grant is keyed to the signing identity, so an ad-hoc daemon would lose it on each update and every wipe would fail. |
| **Who runs an action** | A **pool of unprivileged lease users**, one per concurrent slot: `kbf-pool-0` .. `kbf-pool-<N-1>`. Hidden (`IsHidden`), standard (not admin), no password login, shell `/usr/bin/false`, primary group `kbfpool`, a fixed uid range, from 61000 by default (some services treat uids below 500 as system accounts [I]). `N` is a profile setting (`pool_users`, passed to the daemon as `--pool-users`), default 100 (D1). | Two leases never share a uid, a home, a `/var/folders` entry, a keychain, a launchd domain or a simulator device set. Nothing one lease does reaches the lease beside it. |
| **How an action starts** | Every pool action goes through `launchctl asuser <uid>`, then drops to the slot uid (`setgid`/`initgroups`/`setuid`) in the `as-user` mode `kbf-mac-session/src/macos/gui.rs` already has, then execs. That puts it in the `user/<uid>` bootstrap namespace, where per-user `cfprefsd`, `secd`/`trustd` and CoreSimulator live [I]. The daemon refuses to run an action as uid 0. | A plain `setuid` from root stays in the system namespace and "cannot reach the user's ... services whatever its uid" (`gui.rs:3-6`), so `defaults`, the keychain and `simctl` would fail or pass by luck. |
| **Slots and scheduling** | The daemon reports `N` action slots; the scheduler never places more than `N` action leases. This is new scheduler work: `Resources.vms` is the model, but nothing books or fills it today (`kbf-types/src/work.rs:67-70`). A whole-machine lease needs every slot idle, so the scheduler **drains** the node for it: no new action leases, wait out the running ones. | Without a slot count, a ninth lease would share a uid. Without a drain, whole-machine leases starve while action leases keep arriving. |
| **One volume** | The worker cache, the scratch root and the pool homes are on a **dedicated APFS volume** (one per Mac, made at provisioning), with `.metadata_never_index` and the backup exclusion on its roots. | macOS has no `protected_hardlinks`: on the system volume a slot could `link()` a root-owned cache inode into `/private/tmp`, where no uid sweep finds it and it pins cache bytes. Across volumes that `link()` gets EXDEV, and a home wipe unlinks everything the home holds. Spotlight off keeps `mdworker` from starting under a slot uid. |
| **Clean between leases** | Pool users are reused. After every lease the daemon cleans the slot in **one pass**, in this order (C1 to C11, `slots/clean.rs`): (1) end the uid's launchd domains, `user/<uid>` and `gui/<uid>`, **whole** (never a job by its label: inside a domain that is going down that answers 124, "tearing down"), then poll `launchctl print user/<uid>` until it lists no job but launchd's own (told by where the job's plist lies, under `/System` or `/Library/Apple`, never by its label), waiting out 124, within a bound; (2) remove the uid's crontab and `at` jobs, so no job starts after the kill; (3) **kill every process of the uid**: first `kill(-1, SIGTERM)` and up to `--native-term-grace-ms` (default 2 s, 0 for none) for the uid's processes to end, so that a workload's resource tracker can unlink the POSIX shared memory and semaphores it made (a Mac cannot list those names, so nothing could remove them later); then a child drops to the uid and execs `kill(-1, SIGKILL)` (no Rust runs after `fork()` in the threaded daemon), repeated until two scans 200 ms apart find none, within 100 scans and a deadline; (4) detach the disk images the uid attached (owner from `hdiutil info -plist`) and unmount every mount under its home or `/var/folders` entry; (5) remove its System V IPC objects; (6) clear the user flags (`uchg`, `uappnd`, `nodump`, `opaque`, `hidden`), remove ACLs and make the owner's mode `u+rwx`, on the **roots first** (the home, because a locked directory cannot lose an entry) and then below it, as root; the uid's `/var/folders` entry gets the same clearing **as the uid**, but only **below** the entry's `0`, `C`, `T` and `X`: the entry and those four folders keep the flags (`sunlnk` among them) and ACLs the system gave them, which neither root without Full Disk Access nor the uid can change; below them, ACLs first, then flags and modes, then ACLs again (the uid obeys an ACL that denies a listing, and a locked file keeps its ACL, so each lets the other through; only the last ACL pass is judged), in a child that drops to the uid with `setgroups`, `setgid`, `setuid`, in that order), because the daemon is a launch daemon without Full Disk Access and root can neither clear the flags and ACLs of the entry's `0`, `C`, `T` and `X` nor list them, while their owner can; (7) empty the real home (keychains included) and the uid's `/var/folders` entry, both as the uid (a child that drops to it; what the uid itself cannot open is a protected store of the system and stays, D7), keeping `0`, `C`, `T` and `X` emptied and removing anything else in the entry root; (8) remove the uid's files in `/private/tmp`, `/private/var/tmp`, `/Library/Caches` (a top-level entry there that the system refuses root even to examine, such as Apple's data-vault caches, is skipped with one warning per path) and the `Shared` folder under `/Users`, and launchd's per-uid `disabled` and `loginitems` plists; (9) remove the crash reports about the uid in the crash folders, `/cores` and every `/Users/*/Library/Logs/DiagnosticReports`: a file the uid owns, or an `.ips` report with the uid as the `userID` of its JSON header or body (a text report is attributed by its owner alone, since a newline in a process path can forge its lines; how a report is otherwise attributed is not known), found by walking each folder by descriptor without following a link; (10) give the home back to the user, mode 0700, no flags, and the entry's four folders narrowed **as the uid** to the system's own modes (`0` and `X` 0755, `C` and `T` 0700; narrower stays) and only where one is wider, never widened (the entry's flags and ACLs and the folders' are the system's and are not touched; a folder the pool user does not own, or whose mode the uid cannot set, fails the clean and is named); (11) a second look at the uid's processes, 200 ms after the first; then a **post-clean check**: no process whose effective *or* real uid is the slot's (`PROC_UID_ONLY` union `PROC_RUID_ONLY`, so a setuid-root binary the slot started is seen), checked once more after a short wait; no job in `user/<uid>`; no image of the uid; the home empty, the user's, mode 0700, without flags; the entry's folders empty, looked at as the uid, and the pool user's, no wider than the system makes them; none of the uid's files in the swept places. | A kill by uid is exact: it ignores process groups, `setsid` and pid reuse. Ending the domain first stops the respawn loop. The deadline turns a process stuck in uninterruptible wait into a failed clean, not a hung slot. Every item in (4) and (5) outlives a kill and a home wipe. The flags go before the removal because the shipped order (remove, then clear the flag on the home root) failed on a home locked by its user, and the retry that followed hid it. |
| **When a clean fails** | The user is left **dirty**: the slot is not offered to a lease, the node reports one slot fewer, and the reason is logged (at `ERROR` the first time, at `DEBUG` while it stays the same); nothing is written to disk. There is no retry inside the pass, and the user is never deleted or made again. One **recovery thread** of the daemon is the only thing that cleans a dirty slot again: while any slot is dirty it re-cleans the dirty ones, one at a time, after 5 s, then 10 s, doubling to 5 min while a pass frees none, and starting again from 5 s after a pass that frees a slot or when another slot goes dirty. It needs no lease and no Xcode survey, so a node whose every slot is dirty comes back by itself. Each change in the number of usable slots is pushed to the daemon through its own part of the driver watch (the count and the push under one lock, so an older count never overwrites a newer one), and does not wait for anything else to change. | A failure that still happens has a cause the step order does not remove; it should be seen in the log, not hidden by a second attempt. Deleting a user is an OpenDirectory write (slow, the most likely to fail) and under a user's own processes would leave them running as an orphan uid. A node that takes no lease and reports nothing new would otherwise stay at zero slots until restart. |
| **Sandbox** | Today's `sandbox-exec` profile stays, **as a hermeticity aid only**. It denies file writes outside the lease directory, `/dev`, the slot user's real home and its `/var/folders` entry. The deny rules for the network, `job-creation` and `user-preference-write` go: the clean covers what they protected. `lsopen` stays, and `mach-register` and `file-mount` are added: the clean does not reach an application Launch Services starts outside the sandbox, a Mach service name checked in with launchd, or a file system mounted on a directory. Each has a profile canary that the slot passes before a lease runs ([slot profile canaries](#slot-profile-canaries)). `NO_DEVICE_RULES` stays (`network.rs`). | The write rule still catches a build that writes into the source tree or `/usr/local`. The removed rules only existed because all leases shared one uid, and they break real tests today (`defaults write`, embedded XPC services, the network). |
| **Network** | Open: internet and the LAN. The default for `network` becomes `on`; `network=off` stays as an opt-in (D2); under `off` a connect to the node's own non-loopback address is still allowed, so a service on the node's LAN address stays reachable. | Tests need the internet and the LAN. |
| **Inputs and the worker cache (C2)** | With `--cache-dir` set: the cache is root-owned (directories `root:wheel 0755`, files `0444`/`0555`); inputs are **hard links** to cache entries, as on Linux ([worker-cache.md](worker-cache.md) 5.1); an input that missed the cache is written root-owned and read-only too. Lease users are never root or admin, have no `sudo`, are not in `wheel` or `admin`, get no ACL write, and no cache file has a group or other write bit. **Every directory** of the input tree is made by the daemon and owned by the slot uid, mode 0700; only files are links. **An input that an output names is a private copy** (`clonefile`), never a link, as on Linux. `remove_tree` and the clean only **unlink** an inode the slot does not own; they never `chmod`, `chflags` or strip its ACL. Without `--cache-dir`, inputs keep today's writable modes. | One cache, hard-linked, behaves the same on both platforms; the kernel, not the sandbox, keeps a slot from writing a cache inode. Hit and miss give the same modes, so a result never depends on cache state. Slot-owned directories let SwiftPM write `.build/` and `.swiftpm/` next to `Package.swift`, and `xcodebuild` its `build/` and `xcuserdata`. Atomic replace (write a temp file, rename over a root-owned input, as with `Package.resolved`) works in a slot-owned directory and is intended. |
| **Simulators** | Each lease makes its devices in **its slot user's own default device set** (`~/Library/Developer/CoreSimulator/Devices`). The wipe removes them. The `kbf-lease-` name prefix and the start-up sweep of the shared set go. `KBF_SIMULATOR_UDID` stays. | A lease sees only its own devices; a killed daemon's devices go with the next clean of that slot. |
| **GUI, GPU and Screen Recording leases** | Whole-machine leases, run alone on a drained Mac. **One long-lived auto-login GUI user** per Mac, `kbf-gui` (standard). Before each lease a **health check**: the console user is `kbf-gui`; automation mode needs no authentication; developer mode is on; the Screen Recording grants are present and their re-confirm dates are not near; the template's OS build equals `sw_vers`; FileVault is off; the GPU is visible. A failed check quarantines the node. The daemon starts the action in `gui/<uid>` through `launchctl asuser`. After the lease (D4): (1) kill **every** process of `kbf-gui`, so the session ends and loginwindow returns; (2) restore `kbf-gui`'s home from a golden template, **except** the replayd approvals file, which stays live; (3) wipe `kbf-gui`'s `/var/folders` entry (it holds the LaunchServices database) and its BTM entries, and detach its disk images; (4) `launchctl reboot userspace` back into auto-login. | A GUI session needs a logged-in user, and a new user per lease means a new Setup Assistant run, new per-user TCC rows and new replayd approvals every time. Restoring while the session is live would let Finder, Dock and `cfprefsd` write the old state back at logout, so the session ends first. A template restore of the replayd file would freeze its approval dates and every lease would prompt once the template aged. A FileVault reboot stops at the unlock screen and auto-login never happens. |
| **OS updates** | After an OS update the health check sees the template's build differ from `sw_vers` and refuses GUI leases until the template is captured again (or, later, MDM `SkipSetupItems` covers it). | A stale template brings back the new Setup Assistant panes (`LastSeenBuddyBuildVersion`) on every restore. |
| **Signing** | One farm identity (D5) signs `kbf-daemon`, the apps under test, the XCUITest runners and the farm's recorder. No ad-hoc ("Sign to Run Locally") build gets a TCC grant on the farm. | A TCC grant is keyed to the designated requirement, and an ad-hoc DR changes with every build [V-doc TN3127]. |
| **Screen Recording** | Approved once per Mac, by hand over Screen Sharing, for the fixed farm-signed identities. The grant lives in the system TCC database, which applies to every user [I]. Recording is XCTest's own screenshot and video support, or a small Rust recorder the daemon starts in `kbf-gui`'s session if U4 says XCTest is not enough. **Unattended recording depends on MDM**: macOS 15 and later re-confirm screen-capture approvals periodically [I], and only the MDM restriction `forceBypassScreenCaptureAlert` stops that. Until MDM is live, the health check refuses a recording lease before a re-confirm is due. | MDM cannot grant Screen Recording; it can only deny it, or let a standard user approve it [V-doc TCC schema]. A stable identity is what makes one click last. |
| **MDM (kbf-mdm, waiting on Apple approvals)** | It pushes only: a PPPC `Allow` for Accessibility, PostEvent and AppleEvents for the fixed identities; a PPPC `Allow` for SystemPolicyAllFiles (Full Disk Access) for `kbf-daemon` if U3 needs it; `AllowStandardUserToSetSystemService` for ScreenCapture; `forceBypassScreenCaptureAlert = true`; `SkipSetupItems`; the update deferral that provisioning sets by hand today. It also handles erase and re-enroll of a broken Mac. | Everything but unattended recording works before MDM: each item has a one-time manual equivalent. |

## 2. What breaks today, and what fixes it

All references are to `main` [V-repo].

1. **Every action runs as `_kbf`** (`kbf-driver-native/src/lib.rs`, "Not yet: a per-lease
   user"; `launchd.rs:79`). Everything below follows from that one fact.
2. **Shared temporary folder.** Swift Build's `swbuild.tmp.*`, `xcrun_db` and
   `TemporaryItems` live in `_kbf`'s single `/var/folders/../T`, which the sandbox has to open
   to every lease. A lease can change or remove another running lease's scratch files, and can
   rewrite `xcrun_db`, which the next lease's compiler shims read (`user_folders.rs`, "What
   this gives up"). *Fixed by:* a uid per slot, so each slot has its own `T`, wiped by the
   clean.
3. **Stray processes outlive the lease.** A process that calls `setsid` and is orphaned between
   two polls is never seen and runs on beside later leases (`procs.rs`; `lib.rs`, "Processes
   that leave the tree"). Work handed to `at`, `cron`, a `LaunchAgents` plist or Apple Events
   runs later as `_kbf`. *Fixed by:* bootout, the kill by uid, and the crontab and `at` sweep.
4. **The kill can hit the wrong process.** `procs::kill_all` signals pids from a snapshot. A
   pid recycled in between belongs to the same `_kbf` uid, so it may be another lease's process
   (`lib.rs`, "The signal race"). *Fixed by:* `kill -9 -1` from inside the slot uid.
5. **Shared simulators.** All devices are in `_kbf`'s default set; an action "can reach every
   device of the default set, not only its own", and booted devices run under
   `CoreSimulatorService`, outside the lease's tree (`simulator.rs` header). *Fixed by:* a
   device set per slot user.
6. **Shared keychain and preferences.** `HOME` points into the lease directory (`home.rs`), but
   `cfprefsd` and `securityd` work from the uid's real home [I, U10]. That is why preference
   writes are denied outright (`network.rs` `BASE_PROFILE`), so any test that runs `defaults
   write` fails, and a keychain item one action adds stays for the next lease [I]. *Fixed by:*
   the `asuser` spawn into the slot's own namespace and the home wipe.
7. **Network off by default.** `network` absent means `NO_NETWORK_PROFILE` (`network.rs`).
   *Fixed by:* default `on`.
8. **No worker cache on macOS.** C2 waits on this design ([worker-cache.md](worker-cache.md),
   section 9): every input is fetched and written again for every lease. *Fixed by:* step 5.
9. **Whole-machine leases make and delete a user each time**, through a separate root helper
   reached over a socket, with code-signature caller checks, a used-id ledger, signed admin
   grants and a leak-scan baseline window (`kbf-mac-session/src/lib.rs`, `helper.rs`,
   `grant.rs`), and two userspace restarts per lease. Every fresh user meets Setup Assistant,
   empty per-user TCC rows and a fresh replayd approvals file, so Screen Recording tests cannot
   run unattended. *Fixed by:* one long-lived GUI user with a template restore, run by the
   daemon itself.

## 3. Open unknowns that could change the end state

Each is answered by a probe on one farm Mac, run as a kbf lease or by hand with an operator's
go. A Mac running a probe that reboots or auto-logs in is out of service meanwhile. **U4 runs
first**: it decides whether a recorder exists at all.

| # | Unknown | Probe | If the answer is no |
|---|---|---|---|
| U1 | Do `simctl create`/`boot` and `xcodebuild test -destination` work for a pool user that never logged in, started through `launchctl asuser`? Does a uid at 600 and above with its home on the dedicated volume behave like a `/Users` home? | Run the existing `tests/lease/simulator.rs` flow as `kbf-pool-0`. | Simulator leases go to `kbf-gui` as whole-machine leases. |
| U2 | Does a Metal compute kernel run for a background pool user, with no GUI session? | A Metal compute test as a pool-user action. | GPU compute stays a whole-machine lease. If yes, it becomes an action lease with a `gpu` slot count of 1. |
| U3 | Can root wipe a home that holds TCC-protected folders (`~/Library/Containers`, `com.apple.TCC`) without Full Disk Access? | Wipe a pool user's home after an `xcodebuild test` run, then `kbf-gui`'s after a GUI run. | Grant FDA to the farm-signed `kbf-daemon` once by hand now, by PPPC later; the signed daemon moves up to step 4. Test: update the daemon, then wipe a TCC-protected folder. |
| U4 | Do XCTest screenshots and test-plan video recording on macOS 26 need a Screen Recording grant, or does the Apple-signed `testmanagerd` already have it (and so skip the re-confirm)? | One UI test that takes a screenshot and records video, with and without a grant for the runner. | If they need none, there is no recorder. If they need one, the farm-signed runner gets the one-time approval. |
| U5 | Do a one-click Screen Recording approval, automation mode and developer mode survive rebuilds of the farm-signed binaries and an OS minor update? How long until the re-confirm? | Approve; rebuild; check; install a 26.x update; check all three; read the replayd approval dates. | D6: SIP off in one recoveryOS visit per Mac, with the daemon writing the system TCC database the way GitHub's runner images do [V-doc]. That is undocumented, so the health check gates every GUI lease. |
| U6 | How much does a cold slot cost? The wipe empties `xcrun_db`, the module caches, DerivedData's SDK explicit modules and the per-user dependency caches (SwiftPM, CocoaPods, npm). | Time a lease's first `swiftc`, a cold first `xcodebuild` and a first simulator boot, warm and wiped. | If any is minutes, the clean clones a **warm skeleton** into the home: one the daemon builds as the slot user after each Xcode change. That state comes from the daemon, not a lease, so freshness holds. |
| U7 | How often do our Xcode/Swift actions fail with read-only inputs: writing an input in place, or `codesign`/`strip`/`install_name_tool`/`xattr -cr`/`touch` on a copy that kept the 0444 mode (Xcode's resource copy keeps modes)? | The farm's Mac action corpus with read-only inputs, counting EACCES and EPERM anywhere. | Step 5 ships behind `--cache-dir`, which stays unset on Macs until the corpus is clean. Fix the rules that fail, as on Linux. |
| U8 | Does the clean fail in practice (a busy file, `securityd` holding the keychain), and does `bootout user/<uid>` work for a background domain with no GUI session? | 1000 lease cycles on one slot running `swift build` plus `xcodebuild test`, counting failed cleans. | Fix the cause found in the clean's order; a user is not deleted or made again. |
| U9 | Does "end the session, restore the template, restart userspace" bring `kbf-gui` back logged in every time, with the restored state intact, and how fast? | 200 cycles, timing each and counting misses and state written back. | A light clean without a restart (kill the lease's processes, remove app containers and saved state), with a reboot only when the health check fails. |
| U10 | After the bootout, kill and wipe, does `securityd` or `cfprefsd` still serve the previous lease's keychain items or preferences from memory? | Part of the freshness test (step 4). | Kill the per-uid agents explicitly. The freshness test decides. |
| U11 | Does `confstr(_CS_DARWIN_USER_TEMP_DIR)` give a pool user that never logged in its own `/var/folders` entry? | Ask for it as `kbf-pool-0` after a fresh create, and after a clean. | The daemon calls `confstr` as the slot uid at the end of the clean. The clean never creates the entry itself, so it never has the wrong owner. |
| U12 | Can Background Task Management entries be removed for one uid, without `sfltool resetbtm` for the whole Mac? | Register a login item as `kbf-pool-0`, then remove it by uid. | Reset BTM for the whole Mac during the node drain and reboot of section 1, and refuse login-item tests on action leases. |

## 4. Build order (each step is one PR)

Each step leaves the farm working and is useful by itself. Tests that need root and real
users run on the macOS CI runner (passwordless `sudo`) or as a lease on a farm Mac. Each test
names the defect it catches and the mutant that has to turn it red.

**Step 1. Network on by default.** `network.rs`: absent means `on`; `off` keeps working.
- Test: an action with no `network` property connects to a public host, and a connect to an
  unrouted private address (the LAN stand-in) is not refused by the sandbox; the node's own
  address is not used, because `off` allows it too and the test could not go red. With `off`,
  both are refused, and loopback and the node's own address still connect. Mutant: keep the
  old default.

**Step 2. Pool users and the volume in provisioning.** `kbf-mac-provision` gets new items:
`kbf-pool-<i>` for `i < N` (hidden, standard, shell `/usr/bin/false`, group `kbfpool`, fixed
uids at 600 and above, homes on the dedicated volume); the APFS volume with Spotlight and
backup exclusions; FileVault off; the LaunchDaemon without `UserName`; the key at
`root:wheel 0600`; scratch and cache roots at `root:wheel 0755`. Each has check, apply and
drift detection. No daemon change yet: a root daemon must not ship before step 3, or it would
run actions as root.

The names `kbf-pool-<i>`, the group `kbfpool` and the block of uids (from `pool_first_uid`,
`pool_users` long) belong to the pool. `apply` refuses, and writes nothing at all, when a
record that is not the pool's holds one of them: a user named `kbf-pool-<i>` whose uid is
outside the block, a `kbfpool` whose gid is not `pool_first_uid`, a record that answers to a
pool name as one of several `RecordName` values, or another user or group with an id of the
block. The error names the record; `check` reports the same. A pool record that a half-run
left (the right name, an id not yet set or inside the block) is finished.
- Tests (`tests/check.rs`, `tests/drift.rs` style): a pool user that is admin, has a login
  shell, is in `wheel`, or has a uid below 600 is drift; so is FileVault on, or a pool home off
  the volume. Mutant: drop the admin check.

**Step 3. Root daemon; actions run as slot users through `asuser`; exact kill; slots.** (a)
The daemon takes a free slot per lease and starts the action through `launchctl asuser` and
the `as-user` drop; it refuses uid 0; it reports `N` slots, and the scheduler books them. (b)
The kill execs `/bin/kill -9 -1` as the uid. The keeper switches to uid mode: on end-of-file
it becomes the slot uid and runs the same kill. The memory watch sums the uid's processes. (c)
The sandbox write rule adds the slot's real home and `/var/folders` entry; the `job-creation`
and `user-preference-write` denies go, and `mach-register` and `file-mount` are denied besides. `procs::Tracker` stays for Linux native only.
- **Exact kill.** The action double-forks a `setsid` sleeper, then exits. After the lease no
  process of the uid remains, and a `sleep` started by root, or as another slot, still lives.
  Mutants: kill by process group; kill from root by a pid list.
- **Keeper.** SIGKILL the daemon mid-lease and do not restart it: within 5 s no process of the
  slot remains. Mutant: the keeper in group mode.
- **Parallel leases do not collide.** With `N=2`, two leases at once each write the same names
  under `T/`, the home, `defaults write` and a keychain item, then read them back with
  `defaults read` and `security find-generic-password`. Each sees only its own values. Mutants:
  hand both the same slot; spawn with plain `setuid` instead of `asuser`.
- **Slots.** With `N=2`, a third action lease waits; a whole-machine lease drains the node and
  starts once both slots are idle. Mutants: no slot booking; no drain.
- **Never root.** A lease forced onto uid 0 is refused. Mutant: drop the check.

**Step 4. The clean, the post-clean check, the dirty slot.** The clean of section 1 in
its order, in one pass, with the deadline and both uid filters.
- **Freshness test (the gate of the design).** With `N=1`, lease A leaves a marker in each of:
  the lease directory, the real home, `defaults write`, a keychain generic password, `T/`,
  `C/`, `/private/tmp`, `/private/var/tmp`, `Shared` under `/Users`, a `KeepAlive` LaunchAgent in
  `user/<uid>`, a crontab line, an `at` job, a `setsid` sleeper, a setuid-root child, an
  attached disk image, a POSIX semaphore, a login item, and (after step 6) a simulator device.
  Lease B looks for each through the tool that would use it (`defaults read`, `security`,
  `launchctl print`, `crontab -l`, `atq`, `ps`, `hdiutil info`, `simctl list`) and finds none.
  Each step is needed: with each clean step left out in turn, exactly its own rows are found;
  the kill must follow the bootout (else the agent respawns), and the check must use both
  `PROC_UID_ONLY` and `PROC_RUID_ONLY` (else the setuid-root child is missed).
- **Deadline.** A process the kill cannot remove (a stuck child the test holds) fails the
  clean within the deadline instead of hanging. Mutant: loop with no deadline.
- **A failed clean leaves the user dirty.** Plant a file the clean cannot remove (an immutable
  flag on a root-owned file in the home): the pass fails, the node reports `N-1`, the next lease
  is not given the slot, and once the file can go the recovery thread cleans the slot again
  and offers it, with no lease waiting.
- **A locked home is cleaned on the first pass.** The pool user sets `chflags uchg` on its
  home root; the clean succeeds on its single pass, and does not without the flags step.
- **The entry, the caches and the crash reports.** A file in the `/var/folders` entry root
  outside `0 C T X`, a file the uid owns in `/Library/Caches` and a crash report naming the
  uid under another user's `DiagnosticReports` are gone after the clean; a report naming
  another uid stays, and the report stays without the crash step.
- **Restart.** SIGKILL the daemon mid-lease, restart it: start-up cleans every slot before
  `Hello`.

**Step 5. The macOS worker cache, root-owned, with hard links (C2).** Behind the data directory's `cache/` (`--data-dir`),
which stays unset on farm Macs until U7's corpus run is clean. `worker-cache.md` row C2 moves
from `clonefile` to `linkat` on the dedicated volume; inputs that miss are written root-owned
and read-only; directories are made by the daemon and owned by the slot; output-named inputs
are `clonefile` private copies; `remove_tree` and the clean unlink any inode they do not own.
- **Cache entries unchanged after a lease.** An action tries on every input: open for write,
  truncate, `chmod`, `chflags uchg`, `xattr -w`, `touch -t`, `chmod +a`, rename-then-write.
  Each gets EACCES or EPERM, and every entry's SHA-256, mode, flags, xattrs and mtime are
  unchanged. Mutants: entries owned by the slot uid; a `kbfpool` group-write bit.
- **No link leaves the volume.** The action hard-links an input into its outputs and tries
  `/private/tmp`: the second gets EXDEV. After the clean, `st_nlink` is back to its count
  before the lease. Mutant: cache on the system volume.
- **An output that is an input.** An output path names an input file and the action appends
  to it: the output is right, the entry unchanged (the macOS twin of C-2). Mutant: link it.
- **Input directories are the slot's.** `swift build` in an input package directory succeeds
  and writes `.build/`. Mutant: leave directories root-owned.
- **Hit and miss behave the same.** The same in-place write fails the same way on a hit and on
  a cold miss. Mutant: write miss-path inputs owned by the slot uid.

**Step 6. Simulators in the slot user's device set.** Remove the shared-set naming and the
start-up sweep; the clean removes devices. Needs U1.
- Lease A's device is not in lease B's `simctl list`, on another slot or on the same slot after
  the clean. Mutant: use `_kbf`'s or root's default set.

**Step 7. The GUI user and the whole-machine runtime on it.** Provisioning adds `kbf-gui`,
auto-login (`kcpassword` for now, MDM later), `DevToolsSecurity --enable`, `automationmodetool
enable-automationmode-without-authentication`, the golden template (with its OS build
recorded), and the one-time Screen Recording click. `whole_machine.rs` drops
`user-create`/`user-delete` and the second userspace restart; the daemon calls the
`kbf-mac-session` library in-process (section 5); it runs the health check before each lease
and the four-part GUI clean after. `state.rs` keeps its across-the-restart record. Needs U3
and U9.
- **GUI freshness.** Lease A writes app preferences, a saved-state file, a container file, a
  login item, a LaunchServices registration of its app and an attached disk image. Lease B
  finds none. Mutants: skip the restore; restore before ending the session (state is written
  back); skip the `/var/folders` wipe.
- **Recording approvals stay live.** After a restore the replayd file's approval dates are the
  live ones, not the template's. Mutant: restore the replayd file from the template.
- **Health check.** Turn off automation mode, log in another user, make the template's OS build
  differ from `sw_vers`, or set an approval date past due: the lease is refused and the node
  quarantined. Mutant: a check that always passes.

**Step 8. Signing and recording.** The farm identity on each Mac for `kbf-daemon` (earlier, in
step 4, if U3 needs FDA), the runners and the recorder; a rule that refuses ad-hoc signing for
whole-machine leases; the recorder only if U4 needs it. Screenshots and video come back as
normal outputs.
- A UI test records a video after the runner was rebuilt and re-signed, without a new click.
  Mutant: sign the runner ad hoc.

**Step 9. Delete what the root daemon replaced.** Remove the helper binary, socket, client,
wire protocol, caller check, ledger and grant from `kbf-mac-session` (section 5), with their
install items in `kbf-mac-provision` and the grant use in `kbf-mdm`. The admin-grant path goes
with them (D3). The `_kbf` role-user code goes too.
- Test: `cargo tree` shows no crate depends on the removed modules, and the provisioning drift
  check reports a leftover `_kbf` or helper plist. Mutant: leave the plist installed.

**Step 10 (when Apple approves MDM). kbf-mdm profiles.** The PPPC and restriction payloads of
section 1 replace the matching manual steps, and unattended recording stops depending on the
health check's date guard. The health check stays.

Steps 1 and 2 can go in parallel. Steps 3, 4 and 5 are the core, in that order. Step 6 needs
U1, step 7 needs U3 and U9; the probes run while steps 3 to 5 are in review.

## 5. What gets simpler because the daemon is root

**Fold the `kbf-mac-session` binary into the daemon, and keep the crate as a library** that
`kbf-driver-native` calls in-process.

Why fold the process:
- The helper exists because the daemon was not root (`lib.rs`: "the root helper that gives
  every Mac lease its own throwaway user"). With a root daemon, the socket, the per-connection
  nonce, the caller check, the frame protocol and `client.rs` protect nothing. They are a second
  root process that can be down, stale, or a different version during an update.
- Ordering the helper had to keep across a socket, such as "no process starts for a lease being
  deleted" under one ledger lock (`helper.rs`), becomes ordinary code under one lock.
- Restart handling becomes one thing: start-up cleans every slot and checks the GUI user. Today
  the start-up sweep and the helper's `BootReset` handshake are separate.

Why keep it a library:
- Its `Host` trait (OpenDirectory, launchd, libproc) lets the clean rules be tested
  on every platform with a fake (`helper.rs` header; `tests/admin_fake`).
- `sweep.rs` already does the hard part of the clean: a walk by uid that never follows a link,
  checks device and inode, and clears flags only on the departing uid's own entries.
- `autologin.rs` (`kcpassword`, the login window, the userspace restart) and `spawn.rs` and
  `macos/gui.rs` (start a process as a uid, in `gui/<uid>` or `user/<uid>`) are what steps 3
  and 7 need.

What gets deleted: `server`, `client`, `proto`, `macos/caller`, `ledger` (pool names are fixed
and reused), `grant` (no admin leases, D3; automation and developer mode are set at
provisioning), per-lease `user-create`/`user-delete`, the leak scan's baseline window, and
`reboot-dirty`. The post-clean check replaces the scan. `node_admin`'s `phase0` moves into
`kbf-mac-provision`, which already owns one-time setup. `role-user` goes with `_kbf`.

Other simplifications:
- The node key is `root:wheel 0600`, readable by the daemon alone.
- The keeper, the kill and the memory watch each become one rule: "every process of the uid".
- `user_folders.rs` stops being a list of shared names leases must not trample; the slot owns
  its whole `/var/folders` entry.

## 6. Out of scope

- **Hardening against a hostile action.** No signed reboot tokens, no sandbox-escape matrices,
  no pf rules, no read denies, no admin-grant machinery.
- **Residual risk accepted:** an action runs with open LAN and internet access, as an
  unprivileged user that can read whatever is world-readable on the Mac. If one of our actions
  misbehaves, or a local privilege escalation is used, the Mac can be damaged beyond what the
  clean repairs. The recovery is quarantine, then reprovision (an MDM erase once MDM is live).
  That is acceptable because every action is our own build or test.

## 7. Decisions

Decided:
- **D1. Pool size.** A profile setting (`pool_users`, the daemon's `--pool-users`), default 100 on every Mac model,
  raised per model once U8 shows the clean holds.
- **D2. `network=off`.** Kept as an opt-in: it exists, is tested and costs nothing.
- **D3. Admin leases.** The admin-grant path is deleted with the helper (step 9).
- **D4. GUI clean.** End the session, restore the template, restart userspace, per lease;
  pending U9, whose fallback is the light clean with a reboot only on a failed health check.
- **D7. The system's protected stores.** On a Mac whose daemon has no Full Disk Access, Apple's
  daemons keep data vaults in the pool user's home (`Library/Preferences`) and `/var/folders` entry
  (`0/com.apple.LaunchServices.dv`, `T/com.apple.trustd`, `T/TemporaryItems`) that neither root nor
  the pool user can open, clear or remove. The home and the entry are emptied and looked at **as the
  pool user** (the way a child that drops to the uid does it); an entry below the home, or below the
  entry's four folders, that the pool user itself cannot open (EPERM or EACCES) is **skipped** with
  one warning per path per process, and so is the directory that holds only such entries. The next
  lease runs as the same user and is refused them too, so a lease cannot carry its files to the next
  through one; anything the pool user can open is still removed, and a failure names its path. The
  post-clean check and the slot-ready assertion accept such an entry and fail on anything left that
  the pool user can open. This does not hold against code that can read the stores another way:
  revisit (a VM per lease, or MDM-granted Full Disk Access) before running untrusted code.
- **D7a. What D7 never skips.** A directory the pool user *owns* and cannot open (no `r` or `x`) is first
  given its owner `rwx` back by the pool user, then emptied: a lease cannot keep files past the clean by
  closing a folder it owns. Only an entry whose permission cannot be restored (a store of the system) or
  that another uid owns is skipped. A directory below the four folders (or in the home) that carries
  `sunlnk` or the `com.apple.rootless` attribute is placed by the system like the four folders: kept,
  never given flag, mode or ACL changes, never removed, and emptied (what is in it is the lease's); the
  look and the assertion accept it while it is empty. The clean first deletes every keychain item of the
  pool user, as the pool user (`SecItemDelete` for each class, in the data-protection keychain (a file
  keychain's items are files in the home, and the search list holds the system keychain, which the user may not change), then asks again and fails, naming what is left). A home that is left holding only files under
  `Library/Keychains` (`secd` removes the per-user keychain it made about a minute after the lease) is
  emptied again every 2 seconds for 60 seconds and fails, naming the path, if they are still there: a
  keychain is never skipped. `slot_clean`'s `a_keychain_item_one_lease_stores_is_not_readable_by_the_next`
  shows an item one lease stores is not readable by the next.

Open:
- **D5. The farm signing identity.** A Developer ID (DR = identifier plus team, stable across
  rebuilds; recommended), or one shared Apple Development certificate? And where its key lives
  on each Mac: `kbf-gui`'s keychain, which the template restore must then keep, or a root-owned
  keychain file that actions pass to `codesign --keychain`.
- **D6. SIP off as the fallback.** If U5 shows one-click grants do not last: is SIP off on farm
  Macs (one recoveryOS visit each) acceptable, with the health check as the guard?

## Slot profile canaries

Before a lease is handed a slot, the daemon runs one probe per row below as the slot's user,
under the exact profile generated for the lease (`crates/kbf-driver-native/src/slots/canary.rs`).
A row not refused in its own form sets the slot aside. The profile rows are
`SLOT_PROFILE` and `SLOT_NO_NETWORK_PROFILE` in `network.rs`.

| canary | profile rule | the call | refused as |
|---|---|---|---|
| `write-outside` | `(deny file-write*)` | create a file in `/private/tmp` | `EPERM` or `EACCES` |
| `link-outside` | `(deny file-write*)` | hard-link `/etc/hosts` into the lease | `EPERM` or `EACCES` |
| `mach-register` | `(deny mach-register)` | `bootstrap_check_in` of a service name no job declares | `kern_return` 1100 (`BOOTSTRAP_NOT_PRIVILEGED`) |
| `lsopen` | `(deny lsopen)` | `sandbox_check` of `lsopen`, the question Launch Services asks before it starts an application | denied (`sandbox=1`) |
| `file-mount` | `(deny file-mount)` | `mount(2)` of a `devfs` on a directory of the lease | `EPERM` |
| `launchd-bootstrap` | none: `(allow default)` | `launchctl bootstrap user/<uid>` of a `Background` `KeepAlive` agent whose file is in the lease | `Bootstrap failed: 5` |

A slot user has no login session on a node, so `lsopen` cannot be seen working by opening an
application: `open` and `LSOpenCFURLRef` failed or waited with the rule dropped as with it kept
(measured on the runner). The canary asks the sandbox the question Launch Services asks. The
`file-mount` canary mounts a `devfs` itself: a user may mount one on a directory it owns, and
the runner showed that succeed with the rule dropped and fail with `EPERM` with it kept. On a
node where `vfs.generic.usermount` is 0 a user cannot mount at all, so `EPERM` comes either way
and the canary cannot see a missing rule there; the slot cannot mount either.

For `mach-register` the service name is one no job declares, so with the rule dropped launchd
answers 1102 (`BOOTSTRAP_UNKNOWN_SERVICE`), never success: the sandbox refuses before launchd
looks the name up, with 1100 instead of 1102. The canary depends on that split, and a dropped
rule is reported as a failure (not a refusal of this door), not as an open door: the log reads
`could not run`, with `not a refusal of this door: kern=1102`, and `NOT refused: the call succeeded` is
what a check-in answering 0 reads as. The `lsopen`
canary shows that the rule is in the profile the process runs under, not that Launch Services
refused an open. The `launchd-bootstrap` canary writes its job file with mode 0644, so the
refusal is not a file launchd rejects for its permissions.

Left open on purpose:

- `job-creation`: launchd refuses a bootstrap from any sandboxed process, `(allow default)`
  alone included, so there is no rule to add; the `launchd-bootstrap` canary asserts the
  refusal whatever its cause, and a test shows it refused under a profile with no deny. The row proves that a bootstrap is refused in the slot environment, not that the sandbox is the reason: no profile rule governs it, so no change to the profile turns this canary red.
- `ipc-posix*`: denying it breaks Python's `multiprocessing` and `xcodebuild`.
- `hdiutil attach` with no mount point: it still mounts under `/Volumes` through
  `diskimages-helper`, even with `(deny file-mount)`. The clean's unmount step and the death
  of the helper remove it; the `file-mount` canary names a mount point, which the rule refuses.

Measured once on a hosted macOS runner, not guarded by a kept test: cargo, clang, swiftc, git, a
Python virtual environment and `swift build --disable-sandbox` ran with the three added denies.
Not measured: anything that registers a Mach port by name (`CFMessagePort`, `NSMachPort`, an
XCTest host), tools that attach a disk image on a named mount point, and macOS UI tests, which
`lsopen` denial fails (error -54); a slot is back to not running them.
