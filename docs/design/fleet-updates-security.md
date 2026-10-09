# Fleet updates: the security model

This is the security half of [fleet-updates.md](fleet-updates.md): who may change a
node's software, what each component may do, the keys and credentials involved, the
MDM gate, how a node proves who it is, and what still cannot be defended. Everything
here is **planned** unless a section says "today". Section numbers such as "4.2"
without a file name refer to [fleet-updates.md](fleet-updates.md); numbers prefixed
"S" refer to this document, and "M" to [mdm-backend.md](mdm-backend.md). Markers
**[V]** and **[A]** mean what they mean there.

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
| An operator's security key | nothing by itself; signs an erase request after a touch | authorising one erase (M4) |
| An operator with the admin role | the API | everything the API allows, audited |

### S1.3 What a compromised `kbf-server` can do

- On a node, through `kbf-updater`: install a set CI signed **for that node's pool and
  platform**, not expired, above the node's serial floor (S3.1). It cannot downgrade a
  node, cross pools, or run a command of its choosing.
- Through `kbf-mdm-gate` (S5): force-update one Mac per pool at a time to a build a
  valid signed set names, and install a profile the gate already allowlists, with an
  alert the server cannot suppress. It **cannot erase** a Mac: every erase needs an
  operator's hardware-key signature (M4). It can relay, delay or drop a signed
  request, never make or redirect one.
- Obtain a privileged-lease grant only for a Mac an operator has already signed an
  erase for, with a signed purpose naming that lease (S8, M4.2): each gives root on
  one Mac, alerts natively, and ends in that erase, which the gate runs itself, so
  the server cannot keep the root by never asking.
  Without the grant check in `kbf-mac-session`, a compromised server or daemon could
  create an administrator lease user, and so get root, on every Mac at once.
- Deny service: cordon, drain or hold every node, or withhold operators' erase
  requests so quarantined Macs stay out. That costs availability, not integrity, and
  every write is audited.

### S1.4 Residual risks (not defended here)

- **The MDM's host.** Root on it, or its API key, can erase every Mac and push any
  profile, and can add a key to the gate's allowed signers. It is the Mac fleet's
  root of trust, like the Apple Business Manager administrator account. Where it runs
  is an open decision (S7).
- **An operator's own machine.** Malware there could have the operator touch-sign an
  erase for a Mac other than the one shown. The gate's caps and its alert naming the
  signed serial bound that to at most two Macs a day (the daily cap's default), one
  at a time (M4.3).
- **CI and the main branch.** A compromised CI, or a malicious change that is
  reviewed and merged, can sign a set that gives root on every node of a pool. The
  key that signs automatically covers only unprivileged `kbf-daemon` (S2.2); anything
  that runs as root, the two helpers included, needs the platform key and an
  operator's approval. With the KMS custody of S2.1 and the canary and soak (4.2)
  that narrows the risk to a compromised approval or a malicious reviewed change; it
  does not remove it. A compromised component key gives code execution as the daemon's
  user on every node: no root (S4.3), but it can write wrong results into the cache
  from every node. Only the canary and soak stand in the way.
- **A known-bad set within its validity.** A node that has not yet learned the floor
  that retires a bad set (S3.1) can still be moved to it until the set expires.
- **Background Security Improvements** change a Mac's build, and may reboot it,
  outside any rollout (7.2).
- **A server that skips `Update`** and asks the gate to enforce directly can reboot a
  Mac mid-lease. The gate's caps (S5.2) bound it to one Mac per pool at a time.
- **A VM guest escape** lands as the uid of the VM host process. `macos-vms.md`
  section 6 (open PR [#85](https://github.com/komira-ai/komira-build-farm/pull/85))
  states that `kbf-vmm` runs as a dedicated non-admin uid outside the helpers' group,
  started through `kbf-mac-session run` or its own launchd user, never as `_kbf` (S4.3).
- **Linux join credentials have no hardware attestation.** A Linux node's identity
  rests on the operator's provisioning job (S6); TPM-based attestation is a later
  option, not designed here.
- **Revocation is not instant.** A dropped CI key stays acceptable to a node until the
  key statement it last saw expires (7 days, S2.3).
- **Apple's push certificate** alone only wakes devices, so its theft has low impact.
  A community CSR-signing service sees the CSR, not the private key; its risk is to
  renewal (availability), not integrity. Renewal under a different Apple account
  forces every Mac to re-enroll, so the same account is always used.

## S2. Signing keys

### S2.1 Custody

- The signing keys live in a cloud KMS and never leave it. CI reaches them through
  GitHub's OIDC token, exchanged for a short-lived KMS credential. The trust policy
  pins the token's `repository`, `ref` (`refs/heads/main`), `job_workflow_ref` (the
  signing workflow at `refs/heads/main`) and `event_name` (`push` or
  `workflow_dispatch`). A pull request or a fork cannot sign. The platform key's
  policy also requires the protected GitHub environment, whose deployment branches are
  limited to `main`.
- A deployment without a KMS may keep the keys as GitHub environment secrets limited
  to `main` with required reviewers. That is weaker (the secret is readable by the
  workflow that uses it) and the deployment's documentation says so.

### S2.2 Two keys

| Key | Signs | Who triggers it |
|---|---|---|
| component key | sets that change only `kbf-daemon` (and any later unprivileged kbf part) | every green, attested `main` build, automatically |
| platform key | sets that change anything that runs as root or below: `kbf-updater`, `kbf-mac-session`, the OS, Xcode, the Metal toolchain, VM images, probes, the profile, firmware | a reviewed change, through the protected GitHub environment, after an operator's approval |

`kbf-updater` checks that every artifact a set changes is covered by the key that
signed it: a component-key set that names a new OS build is refused.

### S2.3 Root key, rotation and recovery

- An **offline root key** (ed25519, generated and kept offline by the operator, never
  in CI) is the only key pinned on nodes at provisioning. It signs a short
  **key statement**: the current component and platform public keys, a serial and an
  expiry (7 days; the operator re-signs it weekly offline, and `kbf-alert` warns 2
  days before the newest statement expires). CI publishes the statement with every set; `kbf-updater` accepts a set only
  under a key named by the newest valid statement it has seen, and never accepts an
  older statement.
- **Rotation** is a new key statement signed by the root key. Nodes pick it up with
  the next set; no node is touched by hand.
- **A compromised CI key** is revoked by a key statement that drops it, then every
  pool gets a new set (higher serial) so the floor moves past anything the old key
  signed (S3.1). Until a node sees the new statement, the old one is valid for at most
  its remaining lifetime: that is the revocation latency.
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
4. it is not past its `expires` time (30 days by default). Re-signing a pool's current
   set before then makes a new set with a new serial; for the platform key that needs
   the same operator approval, one click per pool a month;
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
`/Applications`, a new profile), the updater always hands the new expected items to
`kbf-mac-session` through a root-only file before it finishes, and before any reboot,
so the boot scan of the next boot already expects them. The daemon cannot trigger
that hand-over.

### S4.2 `kbf-mac-session`

Mac-only, LaunchDaemon, Full Disk Access granted by an MDM profile at provisioning.
Fixed verbs,
every argument restricted to the lease uid range (default 600-699 **[A]**):

| Verb | Does |
|---|---|
| `user-create <lease> [grant]` | create `kbf-lease-<lease id>` (a name never reused): random password, no secure token, non-admin. An administrator only with a grant the helper verifies itself (S8) |
| `run <lease> <fd-passed argv>` | start the action's process tree as that user; the daemon passes the argument vector and the stdio and lease-directory descriptors over the socket |
| `session-login <lease>` | set auto-login to that user, then a userspace restart (or a full reboot) |
| `session-idle` | clear auto-login, then the same restart: the Mac rests at the login window |
| `kill-uid <uid>` | `launchctl bootout gui/<uid>` and `user/<uid>`, then kill every remaining process of a uid in the lease range |
| `user-delete <lease>` | delete the user and sweep its state (below); refused while `kill-uid` still finds a process of that uid |
| `scan` | the leak scan (10.2) against the stored baseline; returns the diff |
| `baseline` | record the scan baseline; accepted only after a boot that followed an apply or an erase and before the first lease user of that boot |
| `reboot-dirty` | reboot; accepted only when the helper's own last `scan` came back dirty |

- **Sweep on delete.** A uid is reused once the range wraps, so `user-delete` removes
  everything keyed by that uid or name that survives deleting the home: crontab
  (`/usr/lib/cron/tabs`), `at` jobs (`/var/at`), Background Task Management entries,
  per-uid launchd overrides, pending print jobs, and files the uid owns in macOS's
  shared user folder and the temporary folders. That Background Task Management entries
  can be removed per uid is **[A]**. Root's walk never follows a symbolic link (other
  lease users may be active and could plant one): it opens each entry relative to its
  parent without following links, and removes the link, not its target.
- **Lease files owned by another uid.** The unprivileged daemon must read a lease's
  outputs and delete its directory although the lease user wrote them. On Macs the
  lease directory carries an inherited ACL entry granting `_kbf` read and delete, so
  every file the lease user creates inherits it. On Linux the files belong to the
  container's mapped uids; the daemon reads and removes them through `podman unshare`
  (as `main` already removes directories) or an idmapped mount.
- **Which leases get what.** Every Mac lease gets `user-create`, `run` and
  `user-delete`. The session switch (L2) is only for whole-machine leases that need a
  GUI session; the leak scan only for whole-machine leases.
- **At boot**, before `kbf-daemon` may send `Hello`, the helper clears auto-login and
  removes any leftover lease user (sweep, then a scan against a baseline that already
  includes any items an apply handed over, S4.1), so a Mac that lost power mid-lease
  never comes back logged in as a dead lease user.
- **Baseline.** Qualification (4.2 step 8) records the baseline after the update's
  reboot and before its first qualification lease.
- It never touches a uid outside the range or the MDM's managed administrator, and it
  cannot change installed software. `automationmodetool` runs once from the
  provisioning profile, not through this helper.

> **Built (phase P1):** `crates/kbf-mac-session` serves `user-create`, `run`, `kill-uid`
> and `user-delete` with the socket and caller check of S4.3; `session-login`,
> `session-idle`, `scan`, `baseline`, `reboot-dirty` and the boot-time reset are not
> built. Where it settles what this section leaves open, or differs:
> - The user is `kbf-lease-<term>-<seq>` (a hyphen for the lease id's dot). Used lease
>   ids are kept in an append-only ledger in the helper's state directory, so a name,
>   and a grant (which names one lease), is used once across reboots; a uid is free
>   again only once its lease's deletion is recorded, and allocation continues after
>   the uid handed out last.
> - `kill-uid` takes the lease, not a uid: the uid comes from the ledger, so the daemon
>   cannot name another lease's uid, and a deleted lease (whose uid may be reused) is
>   refused. The kill is `kill(-1, SIGKILL)` from a child that took the uid, repeated
>   until `libproc` lists no live process of it by real or effective uid.
> - The grant is the gate's own, one format on both sides (`kbf-mdm`'s `grant`
>   module defines it; S5.2 states it): the five-line text `kbf-grant-v1`, `serial`,
>   `lease`, `issued`, `not-after` (UTC `YYYY-MM-DDTHH:MM:SSZ`, `not-after` exactly
>   an hour after `issued`), Ed25519 over the text by the gate's key. The helper takes
>   `grant-admin`'s `token` unchanged, `<payload>.<signature>` in unpadded base64url,
>   and verifies it strictly under the keys installed on the Mac, never a key the
>   answer names. It refuses a grant whose `not-after` the clock has passed, or which
>   lies more than 65 minutes ahead of it. A test verifies the token the gate's own
>   code signed for its test.
> - The gate's public keys are a file named by a flag (base64, one per line; how the
>   MDM delivers it is P3's). It, the state directory and the ledger must be root's
>   and writable by no one else, and the key file is never read through a link: whoever
>   could write them could make administrators or replay a grant.
> - The sweep covers the home folder, the crontab, `at` jobs, launchd's per-uid
>   `disabled` and `loginitems` files, the shared user folder and the temporary
>   folders. The crontab and `at` jobs go first, and `user-delete` looks for live
>   processes of the uid three times: before anything is removed, once those are gone
>   (a job that fired after `kill-uid` is found there), and after the whole sweep,
>   just before the record is deleted. Each look lists the uid's processes up to 50
>   times, 100 ms apart, so a cron job that started between `kill-uid` and the delete
>   and ends by itself is waited for; a process still alive then refuses the delete,
>   and the refusal names its pid and command. A process whose state cannot be read
>   counts as live. Background Task Management entries live
>   in one system-wide database; no per-uid removal is built, so they stay **[A]**, for
>   the leak scan (P4).
> - **Pending print jobs are not swept.** CUPS keeps a job's files in
>   `/private/var/spool/cups` owned by root, not by the user, so a sweep by owner
>   cannot find them; cancelling the departing user's jobs by name (`cancel -u`) is
>   P4's, and until then they are for the leak scan.
> - On the hosted macOS runner a new lease user is also a member of `_lpoperator`
>   and `com.apple.sharepoint.group.1`, through nested groups. P4's leak scan should
>   record a lease user's group memberships.
> - The password is random and never told to anyone; with no password known, an
>   administrator lease user cannot use `sudo` either. P4's auto-login needs its own
>   way to hand the session a password.
> - `run` starts the process in the system launchd domain as the lease user, not in
>   the user's own domain (`launchctl asuser`); tools that need per-user launchd
>   services are P4's.
> - **The caller check (S4.3) as built.** An earlier build checked the caller only
>   after reading its request, and on the hosted macOS runner a process that connected
>   and then executed the genuine client was accepted: the audit token was fetched
>   after the exec, by which time it named the genuine client. Measured on the same
>   runner (macOS 26.6.2) by the CI job's exec probe: a process's pid version does
>   change on `exec` (15240 before, 15241 after, same pid), and `LOCAL_PEERTOKEN`
>   reports the process as it is when asked, so a token fetched after the exec is the
>   new image's. The helper now identifies and checks the caller as soon as it
>   accepts, before reading anything, sends a fresh nonce the request must carry, and
>   requires the same audit token (pid and pid version) once the request has arrived.
>   The CI job plants both forms, executing after the helper's answer and writing the
>   request first, and fails if either is accepted; it prints the token's pid and pid
>   version at accept and at the request, and what `LOCAL_PEERTOKEN` reports across an
>   `exec`. The case left open is in S4.3.
> - On macOS `kill(-1)` also signals the sender, so the helper's killer child ends by
>   `SIGKILL`; that is its normal end.

### S4.3 Who can reach a helper

Both helpers listen on a Unix socket in a root-owned directory, mode 0750, group a
**dedicated** group (`_kbf` in [mac-node-provisioning.md](mac-node-provisioning.md)), never `staff`, which every macOS user is in;
socket mode 0660. Two controls, the first being the one that matters:

1. **No action, probe or VM runs under a uid in that group.**
   - Mac: every action and probe runs as a lease user via `run`. On `main` the native
     driver runs actions as the daemon's own user (`crates/kbf-driver-native/src/lib.rs`),
     so `kbf-updater` ships on Macs only with the per-lease user.
   - Linux: rootless Podman's default maps a container's uid 0 to the daemon's uid on
     the host, leaving only the mount and pid namespaces to hide the socket. The
     container driver therefore runs every container with `--userns=nomap`
     (`crates/kbf-driver-container/src/podman.rs`), so no container uid or gid maps
     to the daemon's; `--userns=auto` was refused because, rootless, one container
     takes a whole 65,536-id range and the next cannot start ([daemon.md](daemon.md),
     User namespaces). Both helpers refuse to start on a Linux node whose daemon uses
     `--driver native`.
   - VMs: `kbf-vmm` (#85) runs as its own dedicated non-admin uid outside the group,
     started through `kbf-mac-session run` or by its own launchd user, never as
     `_kbf`.
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
     (`LOCAL_PEERTOKEN`), gets the caller's code with `SecCodeCopyGuestWithAttributes`
     and `kSecGuestAttributeAudit`, then checks it with `SecCodeCheckValidity` against
     the `kbf-daemon` requirement (its cdhash) pinned by the installed set. It does
     so as soon as it accepts the connection, before reading a byte; it then sends a
     fresh nonce the request must carry, and when the request arrives the audit token
     (pid and pid version) must be the one it checked. So a caller that connects and
     then executes the genuine daemon is refused: it was checked as itself, and a
     request written before the exec cannot carry the nonce. Left open: a process that
     executes the daemon before the helper accepts while a child it forked keeps the
     connection, where the kernel's token does not follow the child that writes. The
     full answer is a challenge the daemon answers with a key only its own code can
     use (S6's keychain identity); until then control 1 is the defence against it.
   - **Ad-hoc signing and Full Disk Access.** With ad-hoc signatures ([mac-node-provisioning.md](mac-node-provisioning.md)'s lean) the
     MDM's Full Disk Access profile can only pin `kbf-mac-session`'s cdhash, so a set
     that changes `kbf-mac-session` also needs a new profile, pushed through the gate in
     the same rollout step; with a Developer ID signature the profile would name the
     team and survive updates.

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
- **Inventory.** The gate keeps its own inventory per Mac: the serial from Apple
  Business Manager (through NanoDEP), the platform UUID from the Mac's first MDM
  enrollment, and the pool and CPU architecture (`arm64` or `x86_64`) from the
  operator's configuration of the gate. Every verb refuses a serial not in it.
- `enforce <serial> <set>`: applies every check of S3.1 that does not need the node
  (signature under the key statement, the platform key, pool and platform: the set's
  `os` is `macos` and its `arch` is the one the inventory records for the Mac, `expires`,
  and a per-pool serial floor the gate keeps itself from every set it has seen), then
  posts the enforcement for exactly the set's build. Refused while another enforcement
  in the same pool is outstanding, or if the Macs checked in and not being erased or
  updated would drop below the gate's Mac floor.
- `grant-admin <serial> <lease id>`: for a privileged lease (S8), only if the gate
  holds an unused, operator-signed erase request for that serial whose signed
  purpose is `privileged-lease <lease id>` for this lease (M4.2, M4.4), and an erase
  is still within today's cap; it reserves that erase and returns a grant signed with
  the gate's own key, naming the serial, the lease id and a 1-hour expiry. **The gate
  itself schedules `EraseDevice` for that serial** at the grant's time plus the
  maximum privileged-lease duration, under that signed request, and runs it then
  whatever the request's `not-after` (which bounds acceptance only, M4.2); the server
  can only bring it forward, never cancel it, and the Mac does not count toward the
  gate's floor until it re-enrolls.
  The grant key is held only by the gate's uid on the MDM host; its public key reaches
  Macs in an MDM profile. Rotation is a new profile, and Macs accept either key during
  the overlap. A grant is single-use: `kbf-mac-session` records used lease ids across
  reboots. The grant's exact format follows.
- **The admin grant.** `kbf-mdm-gate` defines it (`crates/kbf-mdm/src/grant.rs`);
  `kbf-mac-session` accepts exactly this and nothing else.
  - *Text.* Exactly five lines, each ending in one LF (no CR, no blank line, no
    trailing whitespace), each field separated from its value by one space:

    ```text
    kbf-grant-v1
    serial <serial>
    lease <lease id>
    issued <time>
    not-after <time>
    ```

    `<serial>` is 1 to 32 ASCII letters and digits, the Mac's hardware serial (on the
    Mac, `IOPlatformSerialNumber`). `<lease id>` is 1 to 128 characters of
    `[A-Za-z0-9._-]`. `<time>` is UTC as `YYYY-MM-DDTHH:MM:SSZ` (RFC 3339, whole
    seconds, a literal `Z`). `not-after` is exactly `issued` plus 3600 seconds.
  - *Signature.* Ed25519 (RFC 8032, pure: no prehash, no context) over the text's
    bytes, final LF included, by the gate's grant key. The public key is the raw
    32-byte Ed25519 key.
  - *On the wire.* `grant-admin` answers
    `{"grant", "signature", "key", "token", "erase_at"}`: the text, the 64-byte
    signature and the public key in standard base64 (with padding), and `token`,
    which is `<payload>.<signature>`, each part base64url without padding (RFC 4648
    section 5), the payload being the text's bytes. The server passes `token`
    unchanged as the `[grant]` of `user-create <lease> [grant]` (S4.2); `erase_at` is
    for the server's records only.
  - *What the Mac checks* before it creates an administrator, refusing on any
    failure: the token has exactly one `.` and both parts decode; the signature
    verifies strictly (no non-canonical encodings, no small-order keys) under one of
    the grant keys the MDM installed (two during a rotation); the payload parses as
    exactly the five lines above, in that order, each once; `serial` is this Mac's;
    `lease` is the lease `user-create` names; `not-after` is exactly `issued` plus
    3600 seconds; the Mac's clock is not past `not-after`, and `not-after` is at most
    65 minutes ahead of it (the hour, plus 5 minutes of clock skew); and the lease id
    has never been used on this Mac.
- `withdraw <serial>`: removes an outstanding enforcement.
- `erase <signed request>`: accepted only with a valid, touched signature by an
  operator's hardware-backed key on the allowed-signers list, for the serial the
  signed message names, with a fresh nonce (M4.2). The server forwards it and cannot
  make one. The signed `purpose` decides what the gate does: `erase-now` runs at once
  or is refused; `privileged-lease <lease id>` is held for `grant-admin` and
  discarded, with an alert, if no grant follows within 24 hours. Even with a valid
  signature, **one Mac at a time across the fleet**: an `erase-now` is refused while
  any other erase is outstanding (until that Mac re-enrolls and checks in, or 24 h
  pass), beyond a daily cap (default 2), or below the floor; a scheduled lease erase
  waits for the outstanding one to clear and is never dropped. The cap counts erases
  sent in the last 24 hours plus erases scheduled by a grant and not yet sent;
  `erase-now` and `grant-admin` need that sum below the cap, and a scheduled erase is
  sent whatever the cap and then counts as sent. The sum never exceeds the cap, so no
  24 hours see more erases than the cap. A held request 24 hours old is refused by
  `grant-admin` even before it is discarded.
- `bring-forward <serial> <lease id>`: runs the erase the gate scheduled for that
  granted lease now; refused if the gate issued no grant for it.
- `profile <serial> <digest>`: installs a profile only if a verified signed set or
  the gate host's allowlist names that digest (M2.2).
- Status reads: enrollment, DDM status, last check-in, and Apple's catalogue (M2.2,
  M3).
- Only declarations whose identifier starts `kbf.` are ever created, changed or
  withdrawn by the gate.
- **Alerts of its own.** Every erase, enforcement, grant and profile install is sent
  by the gate itself to the native alerting path and written to its own audit log, so a compromised server
  cannot hide one.

## S6. Enrollment and node identity

- **Join credentials are per device and hardware-attested.** The join credential is a
  certificate issued through DDM's ACME configuration with Apple's hardware-bound
  device attestation (Apple silicon, macOS 14 or later): the private key lives in the
  Secure Enclave and Apple attests the device's serial and UDID. It is issued only for
  a serial in the gate's inventory, and names that serial and platform UUID. That the
  chosen ACME server supports Apple's attestation challenge is **[A]**, to check before
  P3. The MDM's own SCEP enrollment uses a dynamic, per-device challenge, never a
  static one.
- **The daemon uses the key without holding it.** On `main` the daemon loads its
  client identity from PEM files (`crates/kbf-daemon/src/config.rs`, `TlsFiles::load`).
  On Macs it instead signs its TLS handshake through Security.framework with the
  keychain identity (a custom signer), and the key's access is limited to
  `kbf-daemon`'s code requirement. Root, a privileged lease's included, can then use
  the key while it runs on that Mac but never copy it elsewhere (S8).
- **Identity is bound to the credential.** The server takes a node's serial and
  platform UUID from its join credential, not from anything the node reports; a
  `NodeStatus` that disagrees quarantines the node. One serial binds to at most one
  live identity: a second join with that serial is refused and alerts. The gate is
  only ever asked about the credential's serial, so a compromised Mac cannot get
  another Mac erased or enforced through an honest server.
- Linux nodes get their join credential from the operator's provisioning job; it
  names their machine id and, where present, the board serial, with the same one-live-
  identity rule. It has no hardware attestation (S1.4).

## S7. Where the MDM and the gate run: an open decision

Today's ruling takes no server out of the build pool, and every Linux node is also a
worker. The MDM's host holds the APNs certificate, every bootstrap token, the SCEP CA
and the ABM token, so the choice matters:

| Option | What | Risk |
|---|---|---|
| A. Dedicated node | one Linux node runs the MDM and the gate, with no `kbf-server` role, and is excluded from action placement | an exception to "no servers out of the pool" for one node |
| B. On a worker | the MDM and gate run on a worker under their own uid, beside actions | a container escape plus a local root exploit on that node gives the whole Mac fleet |
| C. Outside the farm | a host that is not a node (an operator's admin machine to start, or a small services cluster) on the rack network | root on that host is root over the whole Mac fleet, the allowed-signers list included; an admin machine runs far more software than a dedicated node. Moving it later follows M7 |

**Open decision. Lean: A** among the farm's own nodes, since its cost is one node's
build capacity and B turns one escape into the loss of every Mac. **The reference
deployment starts on C**, on an operator's admin machine, and plans to move to a small
services cluster; the MDM can move between hosts without re-enrolling a Mac, if the
items of M7 move with it. On C the operator's signing machine (M4.2) must not be the
gate's host: if it is, malware there both shows the operator what to sign and edits
the allowed signers, and M4's separation collapses into one machine. Either way, the host never runs
`kbf-server`, and the enrollment endpoints must be reachable on the rack network
(7.6).

## S8. Privileged Mac leases

A privileged lease (`kbf-mac-admin=true`) has an administrator lease user and so can
reach root: it can use (not copy, S6) the node's join credential, replace the helpers, or block MDM
traffic so an erase never arrives. Therefore:

- The front admits `kbf-mac-admin` only from a client with an explicit
  `mac-admin` permission in the server's configuration.
- The scheduler admits a privileged lease only when the gate has erase budget left
  and holds an operator-signed erase request for the Mac whose purpose names that
  lease (M4.2, M4.4); the lease waits for
  that touch before it is placed.
- **The admin user needs a grant the helper verifies itself.** The server asks the
  gate for `grant-admin` (S5.2), which reserves an erase; `kbf-mac-session` creates an
  administrator only if the grant's signature verifies under the gate's public key
  (installed by the MDM at provisioning) and it names this Mac's serial and this lease
  id, unexpired and not used before. The daemon's or the server's word is not enough.
- When the lease ends, the server at once revokes that node's identity and join
  credential and quarantines it, and may ask the gate to bring the erase forward
  (`bring-forward`, S5.2). The erase does not
  depend on that request: the gate already scheduled it with the grant (S5.2). The
  node is readmitted only with a fresh credential, after the gate has seen a fresh
  enrollment of that serial; a Mac whose erase never arrives stays out and alerts.

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
| A caller that connects and then executes the genuine daemon is refused (macOS), whether it executes after the helper's answer or writes its request first | check the caller only after reading its request, and drop the nonce |
| The helpers refuse to start beside `--driver native` on Linux | drop the check |
| A `rollout`-role request with a strategy field is refused | accept strategy from `rollout` |
| The gate refuses an unauthenticated caller, a serial outside its inventory, a second outstanding erase or enforcement, an erase past the cap, and one below the floor | drop the outstanding-erase check |
| The gate refuses an erase the server sends without an operator signature (more in M10) | keep an unsigned `erase` verb for the server |
| The gate alerts on an erase even when the server sends no alert | route gate alerts through the server |
| A node reporting a different serial than at first join is quarantined | accept the new serial |
| A privileged lease's node is quarantined and its credential revoked at lease end | skip the revocation |
| `user-delete` removes a planted crontab, `at` job and login item of the uid, and does not follow a planted symlink out of the shared user folder | follow links in the sweep |
| A probe runs as a lease-range uid, from the installed set only | run probes as the daemon's user |
| `kbf-updater` refuses a component-key set that changes `kbf-updater` or `kbf-mac-session` | let the component key cover the helpers |
| `user-create` makes an administrator only with a valid, unused gate grant for this serial and lease | trust an admin flag from the daemon |
| The gate erases a granted Mac with no erase request from the server | leave the erase to the server |
| `grant-admin` is refused for a Mac with no held, operator-signed erase | grant on erase budget alone |
| `grant-admin` for a lease is refused when the Mac's only signed request names another lease, or was an `erase-now` (more in M10) | ignore `purpose` |
| The gate refuses `enforce` of an expired set or one below its own pool floor | skip the gate's expiry check |
| A second join with an already-bound serial is refused and alerts | bind identity from `NodeStatus` |
| An apply that adds an Xcode is followed by a clean boot scan | hand over the expected items after the reboot |
