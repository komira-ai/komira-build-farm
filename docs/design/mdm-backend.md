# MDM as a backend: what kbf asks of device management

This is the third part of the fleet-updates design, after
[fleet-updates.md](fleet-updates.md) (sections "7.6" and so on) and
[fleet-updates-security.md](fleet-updates-security.md) (sections "S5" and so on).
It says what `kbf-server` asks of a Mac's device management (MDM), how that is a
pluggable backend, why erasing a Mac is not something the server can ask for, what
the MDM needs from the network, how it moves between hosts, and how kbf manages node
configuration itself without a separate configuration-management tool. Sections here
are "M1", "M2", ... Everything is **planned**. Markers **[V]** and **[A]** mean what
they mean in fleet-updates.md; section M10 collects the new **[A]**s.

## M1. Summary

| Topic | Design |
|---|---|
| Interface | `kbf-server` uses three MDM operations, through `kbf-mdm-gate` only: **inventory**, **enforce** a macOS build by a deadline (and withdraw it), **install** an allowlisted profile. |
| Backends | The gate drives the MDM through a backend trait. Default: NanoHUB, self-hosted, open source. Optional: a hosted MDM whose wipe privilege is separate from its other privileges (Jamf Pro). Vendors whose API keys grant wipe together with every other device action do not fit. |
| Erase | Not one of the three operations, and the server cannot create one. An operator action: a request signed with the operator's hardware-backed key (a FIDO2 SSH key), verified by the gate against an allowed-signers list. The server's gate verbs include a relay for such a request and `grant-admin`, which needs one; neither lets the server make or redirect an erase. |
| Progress | On macOS 27 the old software-update commands and queries are gone. Progress comes from DDM status reports the gate polls; nothing announces completion; the node's own `Hello` is the done signal. |
| Network | The MDM is reachable on the network a Mac has at Setup Assistant (the rack LAN is enough), under a DNS name the operator controls, with TLS a freshly erased Mac trusts. No public address; not reachable only over an overlay network. |
| Moving | Keep the DNS name; carry the database, SCEP CA, push certificate and key, ADE token and the gate's keys. |
| Config management | kbf's own: `kbf-updater` with signed sets on Linux; the idempotent provisioning profile plus MDM on Macs. No Ansible. |

## M2. The backend interface

### M2.1 Where it sits

```
kbf-server --mTLS--> kbf-mdm-gate --backend trait--> MDM (NanoHUB, or a hosted MDM)
                         ^
operator's signed erase -+  (relayed by the server, verified by the gate)
```

The gate is not optional; the backend is. Every policy of S5 (inventory, per-pool
serial floor, one outstanding enforcement per pool, Mac floor, daily erase cap, its
own alerts) lives in the gate and holds whichever MDM is behind it. The trait
(`MdmBackend` in crate `kbf-mdm`, 7.6) is the gate's southbound side; `kbf-server`
only ever speaks the gate's protocol.

### M2.2 The three operations

| Operation | Gate verb | What the backend does |
|---|---|---|
| Inventory | `status` | per serial: enrolled, supervised, bootstrap token escrowed, last check-in, OS version and build, the software-update status items (M3), installed profiles |
| Enforce | `enforce`, `withdraw` | post (or remove) one `softwareupdate.enforcement.specific` declaration for one device: `TargetOSVersion`, `TargetBuildVersion`, `TargetLocalDateTime` ([schema](https://github.com/apple/device-management/blob/release/declarative/declarations/configurations/softwareupdate.enforcement.specific.yaml)) **[V]** |
| Install profile | `profile` | install one profile on one device, only if allowlisted (below) |

- **"Update available" is not asked of the device.** The device-side query is gone on
  27 (M3). The gate reads Apple's public catalogue
  ([gdmf.apple.com/v2/pmv](https://gdmf.apple.com/v2/pmv)) at most once a day, as 3.2
  says, and reports each listed build with its posting and expiry dates; the server
  compares it with each node's build. Filtering the catalogue by a Mac's model through
  the catalogue's supported-devices lists is **[A]**. The catalogue host's
  certificate is reported to chain to an Apple root that a Linux host's system trust
  store may not hold, so the gate may need Apple's root configured explicitly **[A]**.
- **Only kbf's own declarations.** Every declaration the gate creates has an
  identifier starting `kbf.` (for example `kbf.osupdate.<serial>`), and the gate
  refuses to change or remove any declaration without that prefix. Settings an
  operator manages by hand in the MDM are out of the gate's reach, and the gate's
  startup reconciliation (4.1) only ever withdraws `kbf.` declarations.
- **The profile allowlist.** A profile is installed only if its SHA-256 is named by a
  signed set the gate has verified (the platform key, S2.2), such as the Full Disk
  Access profile that pins `kbf-mac-session`'s cdhash (S4.3), or listed in a
  root-owned allowlist file on the gate's host. The server cannot send profile bytes:
  it names a digest, and the gate installs bytes it already holds.
- **The server's complete set of gate verbs** is: `status`, `enforce` and
  `withdraw`, `profile`, `grant-admin` (S5.2, S8; the gate defines the admin grant's
  exact format, which `kbf-mac-session` verifies: S5.2 "The admin grant"), and two
  erase verbs that carry no
  authority of their own: `erase <signed request>`, a relay for a request the server
  cannot create (M4.2), and `bring-forward <serial> <lease id>`, which only runs an
  erase the gate already scheduled under a signed request (M4.2).
- **What the server can never ask for:** an erase it did not receive signed (M4),
  lock, setting a firmware or recovery password, removing management, rotating the
  managed administrator's password, any raw MDM command. The gate has no generic
  pass-through.

## M3. Progress on macOS 27

macOS 27 removed the MDM commands `ScheduleOSUpdate`, `AvailableOSUpdates` and
`OSUpdateStatus` and the `com.apple.SoftwareUpdate` payload (7.1, with sources)
**[V]**. What remains is DDM:

- **Status reports, polled.** The Mac sends a DDM status report when a status item it
  is subscribed to changes; the MDM stores the latest values. The gate reads them from
  the MDM and the server polls the gate (every 60 s while a node is `updating`
  **[A]**). The items are `softwareupdate.install-state`,
  `softwareupdate.pending-version`, `softwareupdate.failure-reason`,
  `softwareupdate.install-reason`, and `device.operating-system.build-version` and
  `device.operating-system.supplemental.build-version`
  ([status items](https://github.com/apple/device-management/tree/release/declarative/status))
  **[V]**.
- **No completion event.** An enforcement declaration has no command result that
  says "installed". The node's own `Hello` and `NodeStatus` with the target build is
  the done signal (4.2 step 7); the DDM status only feeds the UI while the node is
  silent and explains a failure.
- **Failure** is a non-empty `failure-reason`, or no `Hello` with the target build by
  the deadline plus the macOS return deadline (3.3). Either holds the rollout (4.3).
- **The MDM must ask for the items.** The gate's backend installs a DDM status
  subscription for these items on every Mac at enrollment; the subscription itself is
  a `kbf.`-prefixed declaration.

## M4. Erase: an operator action

### M4.1 The rule

`kbf-server` cannot make an erase: its only erase verbs relay an operator's signed
request or bring forward an erase already scheduled under one (M4.2), and the gate has
no unsigned erase. Every erase carries
a signature from an operator's **hardware-backed key**: a key whose private half lives
in a security key and signs only after a touch. No server, agent, CI job or node holds
such a key. A compromised `kbf-server` can still relay, delay or drop an operator's
request; it cannot make one.

### M4.2 The signed request

The request is a short text message:

```
kbf-erase-v1
serial <serial>
purpose erase-now | privileged-lease <lease id>
reason <free text>
nonce <128 random bits, hex>
not-after <RFC 3339 time, at most 1 hour ahead>
```

- **Signing:** `ssh-keygen -Y sign -n kbf-mdm-erase -f <key>` with an
  `ed25519-sk` (or `ecdsa-sk`) key on a FIDO2 security key, which asks for a touch
  ([ssh-keygen(1)](https://man.openbsd.org/ssh-keygen)) **[V]**. kbf ships a small
  `kbf-admin erase <serial> --reason ... [--lease <lease id>]` that writes the
  message, runs `ssh-keygen` and posts the result.
- **Verifying:** the gate checks the signature in the format of `ssh-keygen -Y verify`
  against an **allowed-signers** file on its host: root-owned, read-only to the gate,
  each line limited with `namespaces="kbf-mdm-erase"` and optionally `valid-before`
  (same manual) **[V]**. The gate accepts only security-key key types, so a software
  key added by mistake is refused.
- **Touch, checked by the gate itself.** A security-key signature carries a flags
  byte that includes "user present", and a counter
  ([PROTOCOL.u2f](https://github.com/openssh/openssh-portable/blob/master/PROTOCOL.u2f))
  **[V]**; `PROTOCOL.sshsig` defines only the outer envelope. The `ssh-keygen` manual
  does not say that `-Y verify` requires the flag, so the gate reads the flags byte
  itself and refuses a signature without "user present".
- **Checked once, when accepted.** The gate checks the signature, the touch, the
  nonce, the serial and `not-after` once, when the request arrives, and then either
  runs it or holds it. It refuses a message whose serial is outside its inventory or
  whose `not-after` has passed or lies more than 1 hour ahead.
- **Single use:** the gate keeps every nonce it has accepted until that message's
  `not-after` passes, and refuses a repeat; after that the message is refused as
  expired.
- **The purpose decides what happens, not the server.** The server relays every
  signed request through the same verb (`erase <signed request>`); the gate reads the
  signed `purpose` line:
  - `erase-now`: the gate runs the erase at once, within the caps below, or refuses
    it. It is never held or queued.
  - `privileged-lease <lease id>`: the gate **holds** the request for that serial and
    lease and runs nothing yet. `grant-admin <serial> <lease id>` (S5.2) accepts only
    a held request whose serial and purpose name that Mac and that lease; an
    `erase-now` request, or one for another lease, never makes a grant. Once the grant
    is issued the held request is used, and the gate schedules the erase at the
    grant's time plus the maximum privileged-lease duration. **A held request runs at
    its scheduled time whatever its `not-after`**: `not-after` limits when a request
    may be accepted, not when an accepted one runs. If another erase is outstanding
    at that time, the held erase runs as soon as that one clears; it is never dropped.
- **Bringing a scheduled erase forward.** `bring-forward <serial> <lease id>` runs an
  already scheduled erase now (S8). It adds no authority: it is refused unless the
  gate issued a grant for that lease.
- **An unused held request** (no grant follows) is discarded 24 hours after the gate
  accepted it, with an alert; the lease, if still queued, waits for a new signature.
  A held request does not count toward the daily cap until `grant-admin` reserves its
  erase.
- **Caps still apply.** A valid signature does not lift S5's limits: one erase
  outstanding across the fleet, the daily cap, the Mac floor. Raising a cap is a
  change to the gate's configuration on its host, not something a signature does.
  The daily cap counts erases sent in the last 24 hours plus erases scheduled by a
  grant and not yet sent, so no 24 hours see more erases than the cap (S5.2).
- **Re-enrollment.** An erase stays outstanding until the MDM records a status report
  from that Mac later than the second the erase was sent, or 24 hours pass. NanoHUB's
  API exposes no enrollment or check-in time, so this rests on **[A]**: a Mac sends no
  status report between the erase being queued and it running (one that did would
  clear the outstanding erase early; the daily cap and the floor still hold).
- **Alerts.** The gate alerts natively on every accepted, held, discarded and refused
  request, naming the serial, the purpose and the signer. A refusal before the
  signature verifies (a bad or missing signature) names only the serial the server
  gave and the reason: nothing else in the request is the operator's yet.

**The path.** The gate listens only to `kbf-server` (S5.2), so the UI's **Erase**
button (11.2), and a privileged lease waiting for its signature, show the `kbf-admin`
command for that serial (and lease); the operator runs it on their own machine, which
should not be the gate's host (S7), and the signed blob goes to
`POST /v1/macs/{serial}:erase`, which the server forwards unchanged. The server learns
nothing it could reuse: a request names one serial and one purpose, and its nonce is
spent.

### M4.3 Keys

- **At least two keys per operator, or two operators.** A lost or broken security key
  must not stop every repair; a second key kept apart is listed too.
- **Adding or removing a key** is an edit of the allowed-signers file, which needs root
  on the gate's host: the same root of trust as the MDM itself (S1.4).
- **Alternatives considered.** A PIV or OpenPGP smart card signing the same message
  works the same way. An encryption-only identity such as `age-plugin-yubikey` would
  need a challenge-response round trip (the gate encrypts a challenge to it, the
  operator decrypts and returns it); it proves possession but binds nothing to the
  serial unless the challenge names it. Lean: FIDO2 SSH signatures, since OpenSSH is
  everywhere and the message is signed offline.
- **Residual risk.** Malware on the operator's own machine could show one serial and
  have another signed. The caps, the gate's alert naming the signed serial, and the
  floor bound the damage to at most two Macs a day (the daily cap's default), one
  at a time.

### M4.4 What now waits for a touch

| Erase | Before | Now |
|---|---|---|
| Leak that persists after a reboot (L4) | automatic | the Mac is quarantined and alerts; an operator signs the erase |
| End of a privileged lease (S8) | scheduled by the gate with the grant | the grant needs an operator-signed erase whose purpose names that lease; the gate holds it and runs it at the lease's deadline, as before (M4.2) |
| Monthly erase (L5) | scheduled | the UI lists the Macs due; each needs a signature |
| Repair after a bad macOS update (4.3) | automatic | signed |

The cost is a person's touch per erase, and a privileged lease waiting for one before
it is placed. Erases are rare (a few a month across the fleet **[A]**); a fleet wipe
is not.

## M5. Backends

### M5.1 NanoHUB with `kbf-mdm-gate` (default)

NanoHUB (MIT) unifies NanoMDM, NanoCMD and KMFDDM, the DDM server, in one process
([nanohub](https://github.com/micromdm/nanohub)) **[V]**; NanoDEP lists the
organisation's devices from Apple Business Manager
([nanodep](https://github.com/micromdm/nanodep)) **[V]**. Its API takes HTTP basic
authentication with the fixed user `nanohub` and the one API key given on its command
line ([operations guide](https://github.com/micromdm/nanohub/blob/main/docs/operations-guide.md))
**[V]**: there is one key and it can do everything. So:

- The gate runs on the same host under its own uid and is the only holder of the key.
  NanoHUB's API listens on loopback only; the network the Macs use reaches only the
  enrollment, check-in and SCEP paths (M6). The gate refuses to start with a NanoHUB
  address that is not loopback (`localhost`, an IPv4 loopback address, `::1`), since the key
  travels in basic authentication.
- The key gives the gate no more than root on that host already gives (the database,
  bootstrap tokens and push key are there too). The MDM host stays the Mac fleet's root
  of trust (S1.4), and the gate does not add a second one.
- Mapping the three operations onto NanoHUB's KMFDDM declarations and sets and
  NanoCMD's profile install is **[A]**, to read before P3 (14).
- NanoHUB's operations guide does not cover SCEP or serving the ADE enrollment
  profile. Those come from other components: a SCEP server such as
  [micromdm/scep](https://github.com/micromdm/scep), and the enrollment profile served
  from a static URL behind the same reverse proxy (M6) **[A]**.

### M5.2 Jamf Pro (optional, hosted)

For a deployment that already runs Jamf Pro:

- **Wipe is a separate privilege.** Jamf Pro API roles grant privileges one by one;
  the erase endpoint (`POST /v1/computer-inventory/{id}/erase`) needs "Send Computer
  Remote Wipe Command"
  ([privileges](https://developer.jamf.com/jamf-pro/docs/privileges-and-deprecations))
  **[V]**. The gate's everyday API client has a role without it. A second client with
  only that privilege is held by a separate erase executor (its own uid on the gate's
  host) that acts only on a signed request (M4). So the network-facing part of the
  gate cannot wipe a Mac even when compromised.
- **Enforcement** goes through managed software update plans
  (`POST /v1/managed-software-updates/plans`): `updateAction`
  `DOWNLOAD_INSTALL_SCHEDULE`, `versionType` `CUSTOM_VERSION` with `specificVersion`
  and `buildVersion`, and `forceInstallLocalDateTime`
  ([plans API](https://developer.jamf.com/jamf-pro/reference/post_v1-managed-software-updates-plans))
  **[V]** for the schema. **[A]**: that `CUSTOM_VERSION` pins exactly the build named
  for any build the catalogue lists; that `DOWNLOAD_INSTALL_SCHEDULE` is available on
  the deployment's Jamf hosting; how a plan is withdrawn; and that Jamf turns a plan into the same DDM declaration as M2.2.
- Jamf Pro is commercial. kbf works without it; the backend is for those who have it.

### M5.3 Unsuitable: one key for every device action

The gate's model needs one of two things: the MDM's all-powerful key on a host that is
already the fleet's root of trust (M5.1), or a wipe privilege separate from the rest
(M5.2). A hosted MDM whose API keys grant every device action together gives neither:
the gate's host, a separate network-facing machine, would hold a key that can wipe
every Mac. SimpleMDM may be an example: an open customer request on SimpleMDM's
suggestion forum says all device actions, wipe included, share one permission, and
asks to limit wipe to certain accounts and API keys
([restrict wipe](https://suggestions.simplemdm.com/forums/204404-suggestions/suggestions/51108268-restrict-permissions-for-device-wipe)).
That the request exists is **[V]**; that the vendor's API keys still work that way is
**[A]**. Such a vendor becomes usable when it separates the wipe privilege.

## M6. Network

- **Reachable at Setup Assistant.** A freshly erased Mac contacts Apple, then the
  enrollment URL, before any profile, kbf component or overlay client (such as a VPN
  or mesh network) exists on it. So the MDM's enrollment, check-in and SCEP endpoints
  must be reachable on the network the Mac has then: on a rack, its Ethernet LAN. The
  MDM needs no public address. It must **not** be reachable only over an overlay
  network, or an erased Mac never comes back.
- **A DNS name the operator controls** (for example `mdm.example.org`). It is written
  into every enrollment profile, so it never changes (M7). It resolves to the MDM's
  LAN address for the Macs: a record in the LAN's resolver, or a public record
  pointing at a private address. Some resolvers drop public answers that point at
  private addresses (DNS-rebinding protection), so the Macs' resolver is checked
  **[A]**.
- **TLS a fresh Mac trusts.** A certificate from a public CA, or the private CA as an
  anchor certificate in the ADE enrollment profile: Apple's ADE profile object has
  `anchor_certs`, which the device uses "as trusted anchor certificates when
  evaluating the trust of the connection to the MDM server URL"
  ([Profile](https://developer.apple.com/documentation/devicemanagement/profile))
  **[V]** (7.6). A public certificate
  for a LAN-only host is issued with an ACME DNS-01 challenge, which needs a DNS API,
  not inbound reachability.
- **Outbound.** The MDM host reaches Apple's push service, Apple Business Manager and
  the catalogue; Macs reach Apple's push service and update servers. The ports are in
  Apple's enterprise network list
  ([Apple](https://support.apple.com/en-us/101555)) **[A]** (to read before P3).
- **Two faces.** A reverse proxy on the Macs' network passes only the enrollment
  profile, check-in and SCEP paths (served by NanoHUB, the SCEP server and a static
  file, M5.1); NanoHUB's API stays on loopback; the gate's mTLS listener
  is on the interface `kbf-server` reaches, never on the Macs' network (S5.2).

## M7. Moving the MDM between hosts

The MDM may start on a host outside the build pool and move later (to a dedicated node
or a small services cluster on the farm, S7). A move keeps every Mac enrolled only if
it carries all of:

| Item | Why |
|---|---|
| The DNS name | in every enrollment profile; a new name means re-enrolling every Mac |
| The database | enrollments, push tokens, escrowed bootstrap tokens, declarations, DDM status |
| The SCEP CA certificate and key | every Mac's MDM identity certificate chains to it; the MDM checks it on every check-in |
| The APNs push certificate and key | push to every Mac |
| The ADE (ABM server) token and its key | the device list and enrollment profile assignment |
| The gate's keys and state | its grant key (Macs pin the public half, S5.2), mTLS identity, allowed signers, inventory, nonce log, per-pool floors |

**Losing the SCEP CA forces re-enrollment of every Mac**, by hand or by an erase
each **[A]**. **Losing the push key** stops every push until the certificate is
renewed; renewing under the same Apple account keeps the topic and the enrollments,
while a different account forces re-enrollment of every Mac (S1.4) **[A]**. These
items are backed up encrypted, offline, at setup and after every renewal.

A move: lower the DNS record's TTL a day ahead; stop the MDM and the gate; copy the
items; start on the new host; repoint the name; push to every Mac and watch every
check-in arrive. Macs retry check-ins, so a short gap loses nothing **[A]**. TLS
certificates may be reissued; nothing pins them.

On a container platform the MDM is one stateful replica with its database on a
persistent volume and the items above in the platform's secret store; the
loopback-only rule for the API becomes "only the gate's container reaches it".

## M8. kbf's built-in configuration management

kbf manages its nodes' configuration itself, with the pieces this design already has.
There is no separate configuration-management tool.

- **Linux:** `kbf-updater` with signed sets is the only thing that changes a node
  (5.1, 8). Configuration files are artifacts in the set, or part of the bootc image;
  nothing is edited in place on a node.
- **macOS:** `kbf-updater` re-runs the idempotent provisioning profile
  (`kbf-mac-provision apply`,
  [mac-node-provisioning.md](mac-node-provisioning.md)) from a signed set, and MDM does what only MDM can:
  macOS updates, privacy (Full Disk Access) profiles, the managed administrator,
  bootstrap tokens, erase, Lights Out Management.
- **What it shares with a tool like Ansible:** a declared state, idempotent steps,
  staging ahead of time (`kbf-updater stage`, which places a set's artifacts without
  applying them), and drift reported (3.1).

**Why not Ansible:**

- It needs a Python interpreter on every Mac. macOS no longer ships one as part of the
  OS, and Xcode's copy changes with every Xcode rollout **[A]**.
- It cannot do what only MDM can (privacy grants, OS updates on 27, erase), so Macs
  would need MDM anyway, plus a second tool.
- It is another tool holding root on every node, through SSH keys an operator's
  machine or a controller keeps, outside the signed sets, the canary and soak, the
  floor and the audit log; its push model needs an inbound SSH port on every node,
  where kbf nodes need none (4.2 step 5).

## M9. First enrollment of a Mac already in use

A Mac set up before the MDM existed joins once, with a person:

1. Assign it in Apple Business Manager to the MDM server. A Mac bought through Apple
   or a linked reseller is already there.
2. Erase it locally (Erase All Content and Settings, in System Settings) **[A]**. No
   script is needed or supported for this step: the erase needs an administrator on
   that Mac, and nothing kbf ships runs before enrollment.
3. A Mac not yet in Apple Business Manager is added now, at Setup Assistant, with
   Apple Configurator for iPhone (7.6) **[V]**, then assigned as in step 1.
4. Setup Assistant contacts Apple, receives the MDM assignment, and enrolls
   (Automated Device Enrollment, Auto Advance, 7.5). The MDM then installs the
   provisioning package and profiles. From then on the Mac is erased and re-enrolled
   remotely (M4).

Before the MDM is live, a Mac's automatic updates stay off by hand and it restarts
on power return (`pmset autorestart 1`, 12), so its build moves only when the
operator chooses (7.3).

## M10. Tests and assumptions

Tests, each with the planted mutant that must turn it red:

| Test | Mutant |
|---|---|
| The gate refuses an erase with no signature, a bad one, one under a key outside the allowed signers, one in another namespace, and one under a software (non-`sk`) key | accept any key type |
| The gate refuses a replayed nonce, an expired `not-after`, and one more than 1 hour ahead | skip the nonce log |
| The gate refuses a signature from a security key whose flags lack "user present" | ignore the flags byte |
| With only a held request for lease L1 on a serial, `grant-admin` for lease L2 on that serial is refused; an `erase-now` request for that serial runs at once and never makes a grant | ignore `purpose` |
| An `erase-now` request is run at once or refused, never held | hold every request |
| A held request whose `not-after` has passed still runs at the grant's time plus the maximum lease duration | re-check `not-after` when the held erase runs |
| A held request with no grant is discarded 24 hours after acceptance and alerts | keep held requests forever |
| `bring-forward` is refused for a lease with no grant | run any held request on `bring-forward` |
| A signed erase for a serial outside the inventory, past the daily cap or below the floor is refused | let a signature lift the caps |
| A signed erase for one serial, forwarded by the server under another serial, erases nothing | take the serial from the server's request instead of the signed message |
| The gate refuses to change or withdraw a declaration without the `kbf.` prefix | match identifiers by substring |
| The gate installs a profile only by an allowlisted digest, never bytes from the server | accept profile bytes from the server |
| With the Jamf backend, the everyday client's role lacks the wipe privilege (checked at gate start) | start with a role that has it |
| A Mac that reports a non-empty `failure-reason` holds the rollout | wait for the return deadline only |
| A privileged lease is not placed until a signed erase for its Mac is held | grant without a held erase |
| A client presenting `kbf-server`'s certificate without its private key is refused, on TLS 1.3 and 1.2 | skip the handshake signature check |
| A refused request whose signature verified alerts with its signer and purpose | alert with the refusal only |
| With cap 2, a grant's erase sent at t, an `erase-now` at t, and another `erase-now` 16 hours later: the third is refused | count a grant's erase only when reserved |
| A held request 24 hours old is refused by `grant-admin` before the tick discards it | grant from any held request still in memory |
| A set whose `arch` differs from the Mac's in the inventory is refused | check only `os` |
| The gate refuses to start with a NanoHUB address off loopback | accept any `--nanohub-url` |

New assumptions:

| Assumption | Settled by |
|---|---|
| Catalogue filtering by model; the 60 s poll is enough | P3 |
| NanoHUB mapping of the three operations; status subscriptions through KMFDDM | reading NanoHUB, before P3 |
| A Mac sends no status report between an erase being queued and it running (the re-enrollment signal, M4.2) | P3, against a real NanoHUB |
| Jamf `CUSTOM_VERSION`, `DOWNLOAD_INSTALL_SCHEDULE` hosting, withdrawal | a Jamf trial, only if that backend is built |
| The Macs' resolver accepts the MDM's record; Apple's ports | P3 network check |
| Push renewal under the same account keeps enrollments; Macs retry check-ins through a move | reading Apple's references; a test move before P3 |
| Python not part of macOS; local Erase All Content and Settings then ADE | P0 |
| A few erases a month | P4 measurements |
