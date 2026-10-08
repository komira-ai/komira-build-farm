# Fleet updates: the security model

This is the security half of [fleet-updates.md](fleet-updates.md): who may change a
node's software, what each component may do, the keys and credentials involved, the
MDM gate, how a node proves who it is, and what still cannot be defended. Everything
here is **planned** unless a section says "today". Section numbers such as "4.2"
without a file name refer to [fleet-updates.md](fleet-updates.md); numbers prefixed
"S" refer to this document. Markers **[V]** and **[A]** mean what they mean there.

## S1. Threat model

### S1.1 What is protected

- **Integrity of every node:** only software a reviewed change named, signed for that
  node's pool and platform, is ever installed; nothing else runs as root.
- **The cache:** only genuine, enrolled nodes may register as workers and write
  results.
- **The Mac fleet:** no single compromised component short of the MDM's host can erase
  or force-update every Mac.
- **Availability:** a platform stays above its floor; this one is weaker, and the
  residual risks (S1.4) say where.

### S1.2 Actors

| Actor | Can reach | Trusted for |
|---|---|---|
| An action (any lease) | its sandbox, the network it was given; on a bare-metal Mac the rack network, since macOS has no network namespace | nothing |
| A privileged Mac lease (`kbf-mac-admin=true`) | root on that Mac, by design | nothing; the Mac is treated as lost afterwards (S8) |
| `kbf-daemon` | the worker stream, the helpers' sockets | running leases; not for installing software |
| `kbf-server` | every daemon's stream, the gate, the CAS | choosing *when* and *where*; never *what* |
| CI (GitHub Actions) | the signing keys through a short-lived identity (S2) | building and signing sets from reviewed changes |
| The MDM host (NanoHUB and `kbf-mdm-gate`) | APNs, every Mac's bootstrap token | the Mac fleet; the root of trust for Macs |
| An operator with the admin role | the API | everything the API allows, audited |

### S1.3 What a compromised `kbf-server` can do

- On a node, through `kbf-updater`: install a set CI signed **for that node's pool and
  platform**, not expired, above the node's serial floor (S3.1). It cannot downgrade a
  node, cross pools, or run a command of its choosing.
- Through `kbf-mdm-gate` (S5): enforce a macOS build a signed set names for that Mac's
  pool, and erase or force-update **one Mac at a time** per pool, within a daily cap and
  never below the Mac floor, with an alert the server cannot suppress.
- Deny service: cordon, drain or hold every node, and erase up to two Macs a day
  indefinitely (each costs 45-90 minutes of re-provisioning). That costs availability,
  not integrity, and every write is audited.

### S1.4 Residual risks (not defended here)

- **The MDM's host.** Root on it, or its API key, can erase every Mac and push any
  profile. It is the Mac fleet's root of trust, like the Apple Business Manager
  administrator account. Where it runs is an open decision (S7).
- **CI and the main branch.** A compromised CI, or a malicious change that is
  reviewed and merged, can sign a set that gives root on every node of a pool, and for
  kbf's own components the rollout starts by itself. Separate keys (S2.2), a protected
  environment for OS and firmware sets, signing keys that never leave a KMS, and the
  canary and soak (4.2) limit it; they do not remove it.
- **A known-bad set within its validity.** A node that has not yet learned the floor
  that retires a bad set (S3.1) can still be moved to it until the set expires.
- **Background Security Improvements** change a Mac's build, and may reboot it,
  outside any rollout (7.2).
- **A server that skips `Update`** and asks the gate to enforce directly can reboot a
  Mac mid-lease. The gate's caps (S5.2) bound it to one Mac per pool at a time.
- **A VM guest escape** lands as the uid of the VM host process; #85 names that uid,
  and it must be outside the helpers' group (S4.3).
- **Apple's push certificate** alone only wakes devices, so its theft has low impact.
  A community CSR-signing service sees the CSR, not the private key; its risk is to
  renewal (availability), not integrity. Renewal under a different Apple account
  forces every Mac to re-enroll, so the same account is always used.

## S2. Signing keys

### S2.1 Custody

- The signing keys live in a cloud KMS and never leave it. CI reaches them through
  GitHub's OIDC token, exchanged for a short-lived KMS credential whose trust policy is
  bound to this repository, the `main` ref and the one signing workflow. A pull request
  or a fork cannot sign.
- A deployment without a KMS may keep the key as a GitHub environment secret limited
  to `main` with required reviewers. That is weaker (the secret is readable by the
  workflow that uses it) and the deployment's documentation says so.

### S2.2 Two keys

| Key | Signs | Who triggers it |
|---|---|---|
| component key | sets that change only `kbf-daemon`, `kbf-updater`, `kbf-mac-session` | every green, attested `main` build, automatically |
| platform key | sets that change the OS, Xcode, the Metal toolchain, VM images, probes, the profile or firmware | a reviewed change, through a protected GitHub environment that needs an operator's approval |

`kbf-updater` checks that every artifact a set changes is covered by the key that
signed it: a component-key set that names a new OS build is refused.

### S2.3 Root key, rotation and recovery

- An **offline root key** (ed25519, generated and kept offline by the operator, never
  in CI) is the only key pinned on nodes at provisioning. It signs a short
  **key statement**: the current component and platform public keys, a serial and an
  expiry. CI publishes the statement with every set; `kbf-updater` accepts a set only
  under a key named by the newest valid statement it has seen, and never accepts an
  older statement.
- **Rotation** is a new key statement signed by the root key. Nodes pick it up with
  the next set; no node is touched by hand.
- **A compromised CI key** is revoked by a key statement that drops it, then every
  pool gets a new set (higher serial) so the floor moves past anything the old key
  signed (S3.1).
- **A compromised root key** means re-provisioning trust on every node: on Macs, the
  MDM installs a provisioning package with the new root key (or the Mac is erased and
  re-enrolled); on Linux, the operator's provisioning job (the host image for bootc,
  or the install script for pinned packages) installs it, over the BMC console where
  the node is not trusted. That is the cost of a root compromise, and why the root key
  stays offline.

## S3. What a set must satisfy

### S3.1 `kbf-updater`'s checks

A set is installed only if all of these hold:

1. its signature verifies under a key the newest root-signed key statement names, and
   that key covers every artifact the set changes (S2.2);
2. its `pool` and `platform` equal those pinned at provisioning;
3. its `serial` is higher than the installed one (equal and identical is a no-op);
4. it is not past its `expires` time (sets are valid for 30 days by default; CI
   re-signs a pool's current set before then);
5. its serial is not below the node's **floor**: every set carries `min_serial`, the
   lowest serial of that pool still allowed. The updater keeps the highest
   `min_serial` it has seen (from any set it staged) and refuses anything below it. A
   rollback set raises the floor past the bad set, so a node that has seen the
   rollback can never be moved to the bad set; a node that has not is protected by
   expiry (S1.4);
6. every artifact's SHA-256 matches. For a Linux snapshot set the packages' integrity
   rests on apt's archive signature, not on the set: the set pins the snapshot and the
   expected kernel package, and apt verifies what it downloads.

Rollback is never a flag: it is a newer set naming old artifacts (4.3).

### S3.2 Probes come from the verified set

Client-defined probes (6.1) are Mac-only. Their commands and expected values come only
from the set `kbf-updater` verified and installed, which it writes to a root-owned,
read-only file; the daemon reads that file and never takes a probe from the server or
from a set the server forwards. A probe runs as a lease-range user through
`kbf-mac-session run` (S4.2), never as the daemon's own user, so it cannot reach the
helpers.

## S4. The root helpers

### S4.1 `kbf-updater`

A LaunchDaemon on Macs, a systemd unit on Linux, running as root. Three verbs:
`status`; `stage <set>` (fetch and verify, change nothing installed); `apply <set>`
(install, and reboot if the set says the step needs it). There is no `rollback` and no
bare `reboot` verb, so **`kbf-updater`** never reboots a node without a signed set that
requires it. (On Macs, `kbf-mac-session` also restarts the node for session switches
and after a dirty scan; S4.2 limits those.) It keeps a state file (last applied set,
in-progress step), so a crash mid-apply resumes or reports, never guesses.

When an apply changes something the leak scan watches (a new Xcode in
`/Applications`, a new profile), the updater either reboots before the next lease, or
hands the new expected items to `kbf-mac-session` through a root-only file. The
daemon cannot trigger that hand-over.

### S4.2 `kbf-mac-session`

Mac-only, LaunchDaemon, Full Disk Access granted by MDM at provisioning. Fixed verbs,
every argument restricted to the lease uid range (default 600-699 **[A]**):

| Verb | Does |
|---|---|
| `user-create <lease>` | create `kbf-lease-<lease id>` (a name never reused): random password, no secure token, non-admin; admin only for a privileged lease (S8) |
| `run <lease> <fd-passed argv>` | start the action's process tree as that user; the daemon passes the argument vector and the stdio and lease-directory descriptors over the socket |
| `session-login <lease>` | set auto-login to that user, then a userspace restart (or a full reboot) |
| `session-idle` | clear auto-login, then the same restart: the Mac rests at the login window |
| `kill-uid <uid>` | kill every process of a uid in the lease range |
| `user-delete <lease>` | delete the user and sweep its state (below); refused while `kill-uid` still finds a process of that uid |
| `scan` | the leak scan (10.2) against the stored baseline; returns the diff |
| `baseline` | record the scan baseline; accepted only after a boot that followed an apply or an erase and before the first lease user of that boot |
| `reboot-dirty` | reboot; accepted only when the helper's own last `scan` came back dirty |

- **Sweep on delete.** A uid is reused once the range wraps, so `user-delete` removes
  everything keyed by that uid or name that survives deleting the home: crontab
  (`/usr/lib/cron/tabs`), `at` jobs (`/var/at`), Background Task Management entries,
  per-uid launchd overrides, pending print jobs, and files the uid owns in
  `/Users/Shared` and the temporary folders. Root's walk never follows a symbolic link
  (other lease users may be active and could plant one): it opens each entry relative
  to its parent without following links, and removes the link, not its target.
- **Which leases get what.** Every Mac lease gets `user-create`, `run` and
  `user-delete`. The session switch (L2) is only for whole-machine leases that need a
  GUI session; the leak scan only for whole-machine leases.
- **At boot**, before `kbf-daemon` may send `Hello`, the helper clears auto-login and
  removes any leftover lease user (sweep, then a scan), so a Mac that lost power
  mid-lease never comes back logged in as a dead lease user.
- **Baseline.** Qualification (4.2 step 8) records the baseline after the update's
  reboot and before its first qualification lease.
- It never touches a uid outside the range or the MDM's managed administrator, and it
  cannot change installed software. `automationmodetool` runs once from the
  provisioning profile, not through this helper.

### S4.3 Who can reach a helper

Both helpers listen on a Unix socket in a root-owned directory, mode 0750, group a
**dedicated** group (`_kbf` in #76), never `staff`, which every macOS user is in;
socket mode 0660. Two controls, the first being the one that matters:

1. **No action, probe or VM runs under a uid in that group.**
   - Mac: every action and probe runs as a lease user via `run`. On `main` the native
     driver runs actions as the daemon's own user (`crates/kbf-driver-native/src/lib.rs`),
     so `kbf-updater` ships on Macs only with the per-lease user.
   - Linux: today rootless Podman runs containers with no `--userns`
     (`crates/kbf-driver-container/src/podman.rs`), so a container's uid 0 is the
     daemon's uid on the host, and only the mount and pid namespaces hide the socket.
     The container driver therefore adds `--userns=auto` (subordinate uid ranges) or
     `--userns=nomap`, so no container uid maps to the daemon's. Both helpers refuse
     to start on a Linux node whose daemon uses `--driver native`.
2. **Every connection's caller is checked.** This guards against stray binaries and
   pid reuse; it does not stop code that already runs as the daemon's uid, which is
   why control 1 comes first.
   - Linux: `SO_PEERPIDFD` gives a pidfd for the caller, so the pid cannot be reused
     between the check and the request; the uid must be the daemon's and the pidfd's
     executable must hash to the installed `kbf-daemon`. `SO_PEERPIDFD` needs Linux
     6.5 or later; that is the kernel floor for the helpers (Ubuntu 24.04's 6.8 meets
     it), and on an older kernel the helpers do not start. The daemon sets
     `PR_SET_DUMPABLE=0`, so another process of its uid cannot ptrace it.
   - macOS: `kbf-daemon` and both helpers are signed with the hardened runtime,
     without `get-task-allow` and with library validation, so `DYLD_INSERT_LIBRARIES`
     and debugger attachment do not work. The helper reads the caller's audit token
     (`LOCAL_PEERTOKEN`) and validates it with `SecCodeCreateWithAuditToken`; the
     audit token's pid version changes on `exec`, so a caller that connects and then
     executes the genuine daemon is refused. The code must satisfy the `kbf-daemon`
     requirement (its cdhash) pinned by the installed set.

## S5. `kbf-mdm-gate`

### S5.1 Why a gate

NanoHUB's API with the escrowed bootstrap tokens can erase every Mac and enforce any
build Apple signs. Handing it to `kbf-server`, a large network-facing process, would
make a server compromise a fleet wipe. The gate is a small separate process with its
own uid on the MDM's host; only it holds the API key. The alternative, the server
holds the API and the design says so, costs nothing to build and was rejected.

### S5.2 Its surface

- **Caller authentication.** mTLS, and only the client certificate of `kbf-server`
  (pinned by its public key) is accepted. It listens only on the interface the server
  reaches, never on the network the Macs enroll on; lease users on bare-metal Macs
  have the rack network, so an unauthenticated gate would be theirs to call.
- **Inventory.** The gate has its own provisioned inventory: serial, platform UUID and
  pool for each Mac, from Apple Business Manager through NanoDEP. Every verb refuses a
  serial not in it.
- `enforce <serial> <set>`: verifies the set's signature and key statement (S2), that
  the set's pool is the serial's pool, and that it uses the platform key; posts the
  enforcement for exactly the set's build. Refused while another enforcement in the
  same pool is outstanding, or if the Macs checked in and not being erased or updated
  would drop below the gate's Mac floor.
- `withdraw <serial>`: removes an outstanding enforcement.
- `erase <serial> <reason>`: refused while another erase is outstanding (until that
  Mac re-enrolls and checks in, or 24 h pass), beyond a daily cap (default 2), or
  below the floor.
- Status reads: enrollment, DDM status, last check-in.
- **Alerts of its own.** Every erase and enforcement is sent by the gate itself to the
  native alerting path and written to its own audit log, so a compromised server
  cannot hide one.

## S6. Enrollment and node identity

- **Join credentials are per device.** After enrollment the MDM installs a join
  credential issued for that device only, and only for a serial in Apple Business
  Manager (through NanoDEP). The SCEP server uses a dynamic, per-device challenge,
  never a static one, so a device that is not in the inventory gets no certificate.
  Whether the MDM also checks Apple's signature on the device information is **[A]**,
  to read in NanoHUB before P3.
- **Identity is bound at first join.** The server binds each node identity to the
  hardware serial and platform UUID its first `NodeStatus` reports, and refuses any
  later change of either (the node is quarantined and an operator decides). The gate
  is only ever asked about the bound serial, so a compromised Mac that reports another
  Mac's serial cannot get that Mac erased or enforced through an honest server.
- Linux nodes get their join credential from the operator's provisioning job; the same
  binding applies to their machine id and, where present, the board serial.

## S7. Where the MDM and the gate run: an open decision

Today's ruling takes no server out of the build pool, and every Linux node is also a
worker. The MDM's host holds the APNs certificate, every bootstrap token, the SCEP CA
and the ABM token, so the choice matters:

| Option | What | Risk |
|---|---|---|
| A. Dedicated node | one Linux node runs the MDM and the gate, with no `kbf-server` role, and is excluded from action placement | an exception to "no servers out of the pool" for one node |
| B. On a worker | the MDM and gate run on a worker under their own uid, beside actions | a container escape plus a local root exploit on that node gives the whole Mac fleet |

**Open CEO decision. Lean: A**, since its cost is one node's build capacity and B
turns one escape into the loss of every Mac. Either way, the host never runs
`kbf-server`, and the enrollment endpoints must be reachable on the rack network
(7.6).

## S8. Privileged Mac leases

A privileged lease (`kbf-mac-admin=true`) has an administrator lease user and so can
reach root: it can copy the node's join credential, replace the helpers, or block MDM
traffic so an erase never arrives. Therefore:

- The front admits `kbf-mac-admin` only from a client with an explicit
  `mac-admin` permission in the server's configuration.
- The scheduler admits a privileged lease only when the gate has erase budget left.
- When the lease ends, the server at once revokes that node's identity and join
  credential and quarantines it, then asks the gate to erase it. The node is
  readmitted only with a fresh credential, after the gate has seen a fresh enrollment
  of that serial; a Mac whose erase never arrives stays out and alerts.

## S9. Credentials and roles

| Role | Holder | May |
|---|---|---|
| `admin` | operators | every `/v1` write, including `accept_outage`, a lower `min_serving`, `canary=0` and other strategy overrides |
| `rollout` | CI | `POST /v1/rollouts` with only `target` (a signed set digest) and a pool `selector`; the strategy is the server's per-pool policy |
| `daemon` | each node's join credential | the worker stream only |

The `rollout` credential is short-lived: CI exchanges its GitHub OIDC token, bound to
repository, `main` and the workflow, for a token valid for minutes. A request in the
`rollout` role that carries any strategy field is refused. Today the only roles are
`Daemon` and `Client` (`crates/kbf-meta/src/model.rs`); all of this is planned.

## S10. Tests

Each with the planted mutant that must turn it red:

| Test | Mutant |
|---|---|
| `kbf-updater` refuses a replayed older set, and one below the floor | skip the serial or floor check |
| `kbf-updater` refuses an expired set | skip the expiry check |
| `kbf-updater` refuses a set for another pool or platform | skip the pool check: a Mac set applies on a Linux test node |
| `kbf-updater` refuses an unsigned set, one signed by a key no root statement names, and a component-key set that changes the OS | skip key coverage |
| An action is refused at each helper's socket | socket 0666; action as the daemon's uid; caller check removed |
| A container action's host uid is not the daemon's | drop `--userns` |
| A caller that connects and then executes the genuine daemon is refused (macOS) | check the cdhash by pid instead of the audit token |
| The helpers refuse to start beside `--driver native` on Linux | drop the check |
| A `rollout`-role request with a strategy field is refused | accept strategy from `rollout` |
| The gate refuses an unauthenticated caller, a serial outside its inventory, a second outstanding erase or enforcement, an erase past the cap, and one below the floor | drop the outstanding-erase check |
| The gate alerts on an erase even when the server sends no alert | route gate alerts through the server |
| A node reporting a different serial than at first join is quarantined | accept the new serial |
| A privileged lease's node is quarantined and its credential revoked at lease end | skip the revocation |
| `user-delete` removes a planted crontab, `at` job and login item of the uid, and does not follow a planted symlink out of `/Users/Shared` | follow links in the sweep |
| A probe runs as a lease-range uid, from the installed set only | run probes as the daemon's user |
