# Fleet: provisioning and operating the hardware

This document says how an operator brings up and keeps a farm of about 3 server and storage nodes,
about 20 Linux build hosts and about 6 Mac Studios, and how the same design holds at a thousand. It
is a design: **planned** unless a section says "today". The operator learns three things: the
**registry** (the server's durable list of nodes, which hosts add themselves to), one command
(`kbf-deploy`) and one place to look (`kbf-deploy status`). Nobody maintains a file of machines.

It builds on [fleet-updates.md](fleet-updates.md), [fleet-updates-security.md](fleet-updates-security.md),
[mdm-backend.md](mdm-backend.md), [mac-node-provisioning.md](mac-node-provisioning.md),
[capabilities.md](capabilities.md) and [daemon.md](daemon.md).

## 1. Summary

| Question | Answer |
|---|---|
| Source of truth for "which machines exist"? | The **node registry** in `kbf-server` (4.2). Hosts enroll themselves at first boot and appear *pending*; an operator or a policy approves them. A file is at most an optional import/export for bootstrap and disaster recovery. |
| What is typed per machine? | Nothing required. CPU, memory, disks, SSD paths and GPUs are detected on every connect and used by the scheduler as reported (6.1). Config holds policy and rare overrides. |
| What identifies a machine? | The **hardware id** is the permanent key. The **human name** (for example `build1`, `mac1`) is written into the hardware by the operator at first boot (BMC FRU asset tag and/or SMBIOS chassis asset tag), read by iPXE and enrollment, recorded in the registry against the hardware id, and used by logs, labels, scheduling, the CLI and the certificate (4.3 to 4.5). |
| How does Linux get an OS? | Site DHCP (UniFi options 66 and 67) points at iPXE; iPXE chains to an HTTP endpoint on kbf that decides per machine: discover, install image X, boot local, or rescue (5.1, 5.2). |
| How does a Mac get set up? | No PXE (Apple Silicon cannot netboot). ABM assigns the serial to the farm MDM; the MDM sets the device name and pushes a signed `.pkg` (5.4). |
| What is generic, what is ours? | kbf ships mechanisms; the operator's own repository holds names, network, images, approval policy and key locations as config (section 7 lists which is which). |
| Config and updates later? | Nodes pull a signed fleet config and their own record; `kbf-deploy rollout` does canary, soak and rollback (6.3, section 8). |

Principles: a fact hardware or the OS can report is never typed; a fact only a human knows (intent,
approval) is typed once, in policy or the registry, never in a unit file. Flags, not environment
variables, in every rendered unit. Secrets never rest on a build host except its own node key.
`install.sh` and `install_units.sh` (each copies a binary, writes a unit, and holds its own copy of
the node id, labels and server address) are replaced by `kbf-deploy node apply`, which renders the
same files from the node's record (section 11).

## 2. Components

| Component | Runs on | Installed by |
|---|---|---|
| `kbf-server` with the registry and enrollment listener; `kbf-ca` (own user and socket); MinIO; envoy | server hosts | `node apply` |
| boot service (`kbf-deploy boot serve`: TFTP for the iPXE binary, HTTPS boot endpoint, image catalogue) | hosts with `services = ["boot"]` | `node apply` |
| MDM: NanoHUB, NanoDEP, SCEP, `kbf-mdm-gate` | the host with `services = ["mdm"]` | `node apply` |
| `kbf-daemon` / `kbf-all-in-one` | builders / all-in-one hosts | `node apply` |
| node agent (`kbf-updater` on Linux; the macOS agent does not exist yet, 14) | every host | first-boot unit / `.pkg` |
| Outside kbf: DHCP, DNS, NTP, switch and VLAN, BIOS, BMC network, ABM, Apple Developer ID | site | the operator (5.6) |

## 3. What exists today

Checked against the code: `kbf-deploy` is a Linux-only binary with five kits and about 20 verbs
(`linux` always installs server and daemon together). `kbf-mac-provision` applies Mac host settings
and renders the plist but installs no binary and makes no certificate. `kbf-mdm-gate`, NanoHUB, SCEP
and NanoDEP exist. `kbf-updater` is Linux only; **no macOS node agent exists**. `RolloutDriver`
stops at `updating` with an in-memory store: nothing rolls out real software. `--node-id` is a typed
string and the server requires the client certificate to carry exactly one DNS name equal to it; the
node registry is in memory and lost on restart, while the daemon already reports detected arch, OS,
CPUs, memory, GPU and ISA on each Hello. Certificates come from a shell script, with no enrollment
endpoint, rotation or revocation beyond a hand-edited deny-list. No PXE, netboot or autoinstall
exists. The server is a single server with a single-voter metadata log (HA is open in ha.md).

## 4. Registry, identity, enrollment and trust

### 4.1 Authorities and keys

- **Fleet root** (ECDSA P-256), made by `ca init` on an air-gapped machine or hardware key. It signs
  only intermediates, operator keys, the release key and the revocation signer, offline: the online
  side writes a request file, the operator carries it on removable media to `ca sign`. Nodes also
  trust a **standby root** from the same ceremony, kept elsewhere; a lost root is recovered by
  promoting it (a config signed by the current root). The drill is run once before production.
- **Intermediates**: one per server host, generated by `kbf-ca` (separate user and socket, so the
  enrollment listener cannot read the key), root-signed offline, mode 0600, TPM-sealed where
  possible, **name-constrained** (fleet DNS suffix, `clientAuth` only), one-year lifetime. Nothing is
  replicated; compromise of one server revokes that intermediate only.
- **Operator keys**: delegated, one per operator on a hardware token, root-signed, one-year expiry
  that nodes enforce. They sign the fleet config, server-role approvals and rollbacks.
- **Release key**: held by CI, root-signed. A node checks the binary's signature **and** that its
  hash equals the one in the config. **No rollback**: config version and release sequence are
  monotonic; going back is a new config with a rollback entry an operator signs.

With root on a server an attacker can issue certificates for any fleet name from that server's
intermediate, impersonate a build host and read inputs and the CAS. It cannot push software and
cannot make a node a *server* (that needs an operator signature, 4.6). Issuance is audited (4.8);
revoke the intermediate with a root-signed entry. Accepted (decision 5).

### 4.2 The registry

A table inside `kbf-server`, durable in the metadata log (single-voter today, three-voter with HA),
one record per machine, keyed by hardware id with a unique index on name.

| Field | Source |
|---|---|
| `hwid`, serials, MACs, asset tag as read | reported by the host |
| `name` | assigned (4.5) or claimed from hardware and confirmed |
| `state`: `pending`, `approved` (armed or not), `active`, `retired` | the approval flow (4.6) |
| `role`, `services`, OS-disk plan | set at approval; `server` role and disk plan operator-signed |
| `labels` | from profile rules, plus rare overrides |
| `facts`, `facts_history` | detected on every connect (6.1) |
| first/last seen, approver (key id or policy rule id), certificate serials | recorded |

**Why not a file of machines.** A file in git works at 30 hosts and fails past it: every add is a
human edit and a merge conflict, the file drifts from reality, and facts a host can report (its
CPUs) become stale copies. The registry is written by the farm as hosts appear, is queried by key,
pages its listings, and costs a few hundred bytes of identity plus the latest facts per node
(a thousand nodes is about one megabyte). The operator edits *policy*, which does not grow with the
fleet. Durability is a prerequisite: `/enroll` is off until it exists, because a restart would
otherwise un-revoke everyone and reset "used" bits.

**Optional import/export.** `nodes export` writes the registry as JSON, `nodes import` loads one.
Two uses only: seeding the first server (the registry does not exist yet, 15) and disaster recovery
(a periodic export lands in the object store and with `backup pull` on the operator's machine).
Import creates `pending` or `retired` records and never overwrites a live one. It is not what an
operator maintains.

### 4.3 Identity: hardware id is the key, the name is for humans

| Platform | Key | Fallback | Why not the usual |
|---|---|---|---|
| Linux | `/sys/class/dmi/id/product_uuid` as `dmi:<uuid>` | `board_serial`, then `chassis_serial` | `machine-id` changes on reinstall; MACs change with a NIC swap |
| Mac | serial as `mac:<serial>` (ABM and the MDM know it) | none | `IOPlatformUUID` is a recorded second check |

The hwid is printed on labels and readable by root, so it proves nothing. Proof is the **bootstrap
token** (4.6) or the **MDM identity** (4.7); `/enroll` never accepts a hwid alone. A spoofed DMI
value on a host holding a valid token is the residual risk (TPM attestation is deferred).

- Lower-cased; placeholders (zeros, all `f`, `To Be Filled By O.E.M.`, `Default string`) leave the
  host `pending` with a reason, never keyed.
- **Byte order**: SMBIOS UUIDs are mixed-endian; iPXE `${uuid}` and kernel `product_uuid` must
  agree. The boot service canonicalizes both, and acceptance of **one box per hardware model** boots
  it through iPXE and the installer and compares them.
- **Duplicates**: a second host reporting a hwid a live node owns is refused and alerts.
- CI virtual machines may use `static:<name>` with `trust = "test"`, issued from a separate
  intermediate constrained to `*.test.<suffix>`.

The name is chosen once and never derived from the hwid afterwards. It goes into the certificate
(the one DNS name the server already requires), `--node-id` and every log line. Reinstall and disk
swap change nothing. A motherboard swap changes the hwid: `replace <name>` revokes the old
certificate by serial and lets the new board claim the name after approval; labels and history
follow the name.

### 4.4 Where the name lives in hardware

The operator names each machine by hand. When racking a Linux host, before any OS exists, the
operator writes its human name (for example `build1`) into the hardware's asset tag: in the BIOS
setup (SMBIOS chassis asset tag) and/or over the BMC with `ipmitool fru edit` (FRU Product Info
asset tag). iPXE reads it as `${asset}`, the enrollment initrd reads it from
`/sys/class/dmi/id/chassis_asset_tag`, and the registry records it against the hardware id at
enrollment. The registry stays authoritative for identity; the tag is how a person at a rack, an
iPXE script and a fresh OS agree on the name, and how a lost registry is rebuilt by reading machines.

| Platform | Carrier | Written by | Read by |
|---|---|---|---|
| Linux | **SMBIOS chassis asset tag** | the operator, by hand, in the BIOS setup (always available) | unprivileged at `/sys/class/dmi/id/chassis_asset_tag`; iPXE as `${asset}` |
| Linux with a BMC | **FRU Product Info asset tag** (or a chassis-area custom field where the BMC allows it) | the operator, by hand, with `ipmitool fru edit` over the BMC LAN or locally over `/dev/ipmi0` | BMC and `ipmitool fru print` (root) |
| Mac | MDM `DeviceName` plus the registry's serial-to-name map | the MDM, at approval | `scutil --get ComputerName` |
| neither | the registry only | | |

Per-hardware runbook, run once per machine at first boot:

1. Choose the name (the next free `build{n}`, `mac{n}` and so on; 4.5 gives the shape).
2. Write it into the tag: BIOS setup, chassis asset tag; or `ipmitool fru edit <id> field p 4 <name>`
   (Product Info asset tag) over the BMC. Either is enough; setting both is fine.
3. Read it back (`ipmitool fru print`, or boot and read `chassis_asset_tag`), then power on with a
   blank boot disk (it falls through to PXE, 5.1). The
   host enrolls and appears *pending* with its name and hardware id; approval binds them (4.6).
4. After the first install, verify once per machine that the firmware boot order is still local disk
   first (`efibootmgr` or the boot menu): the Ubuntu installer registers its own UEFI entry and may
   reorder. Fix it in the BIOS if not (5.6).

- The tag is a **hint, not proof**. A tag that no live node owns starts a *claim* the operator
  confirms (or a policy does, 4.6). A tag naming a node bound to another hwid holds the host with a
  conflict (cloned disk, mislabeled box). If the tag disagrees with the registry for the same hwid,
  the registry wins and the tag is reported as drifted.
- Optional fallbacks for a **blank tag** (today `Default string` on the current boards): the
  registry assigns a name by policy at approval (4.5), and `kbf-deploy name write` may write it
  back over IPMI as a convenience. A failed write-back never blocks the node; `status` shows
  `tag: unwritten`. Neither is the main flow.
- BMC LAN credentials are the operator's secret, never in the registry; the config names where they
  come from (7).
- FRU writability on the Supermicro X10-generation BMCs is not a prerequisite: the SMBIOS tag can
  always be set by hand in the BIOS. FRU editing via `ipmitool` is a convenience, and the chassis
  area has only part number, serial and custom fields, so the Product Info asset tag is the field
  to use.

### 4.5 Naming policy

The fallback for a blank tag (4.4): the operator hand-writes names in the normal flow, and this engine only assigns one when the tag is empty. A generic engine in kbf, configured by rules in the operator's repository. A rule is a match over
detected facts (platform, CPU model, chassis product, later a rack tag), a template and a counter;
the first match wins.

```toml
# naming.toml  (operator-owned; example only)
[[rule]]
match = { platform = "linux", cpu_model = "E5-2*" }
template = "build{n}"
[[rule]]
match = { platform = "macos", model = "Mac Studio" }
template = "mac{n}"
```

`{n}` is the next integer of that template's counter, allocated atomically with the approval.
Retired names are never reissued (a log line must mean one machine) unless `rename --reuse`. An
approver may override (`approve --name build7`). Names match `^[a-z][a-z0-9-]{0,40}$` and cannot
collide. The example has no server rule: day 0 hand-names the first server through `nodes.json` (15), and later servers are named by the approver (`approve --name`) until the server-prefix question (16, decision 2) is decided; a rule can then be added. A host no rule matches stays pending with "no naming rule": the engine never invents one.

### 4.6 Enrollment: pending, approval, one-shot arming

**First contact.** An unknown Linux host gets the **discovery** image from the boot endpoint (5.2): a
small read-only initrd that detects facts (CPU, memory, disks by id, NICs, GPUs, BMC, asset tag),
POSTs them to `/register` and waits. The record appears `pending`; no certificate or token exists. A
Mac appears `pending` when it first checks in to the MDM (5.4). Reports about unknown machines are
rate-limited, capped per source (16 pending entries per source address) and overall (256, oldest dropped) and shown only to authenticated operators. Every dropped entry writes an audit line (source, hwid if given, reason: cap or rate) and increments a counter, so a flood is visible even though the entries are gone.

**Approval** moves `pending` to `approved`, by:

- an operator: `nodes --pending` shows `serial S, mac M, uuid U, seen at <ip>, proposed build3,
  OS disk nvme-example-os`; the operator checks it against what was racked and runs
  `approve <hwid>` (`--all-pending` lists once and confirms once); or
- a **policy** in the operator's `approval.toml`, for roles that hold no power (for example "daemon
  on the provisioning VLAN, naming rule matched"). The default is manual: auto-approval means
  anyone who plugs a box into that VLAN gets a build host, so rules should require something hard to
  guess and the 5.6 controls still apply.

Approving the **server** role, `services` or any server's disk plan is always manual and
**signed by the operator's delegated key**; the registry accepts that role only with the signature, so
a compromised server cannot mint a server. A Linux approval records the exact `os_disk` by-id path a
disk rule chose from the discovered facts (for example the smallest non-removable NVMe) and shows it
to the approver. The installer may touch only that disk and refuses to start if it is absent;
every other disk is never in its storage config.

**Arming.** Approval, and `reinstall <name>` for an existing node, arms the host: one install,
one-shot, expiring in 2 hours. The boot order is local disk first (5.1), so an installed host never
reaches the network at all; arming is the second guard for a stray network boot (a one-time BMC
or menu boot, a disk that stops being bootable): a not-armed host is told to boot local, so a power
loss never reinstalls a MinIO node and a stray reboot does nothing. The bootstrap
token is minted at arming, stored in the registry and delivered once:

1. The installer fetches `GET /seed/<hwid>` over TLS pinned to the root (embedded in iPXE). The first
   fetch for an armed host returns the seed and consumes the arming; a later fetch is a 409 and an
   alert showing the first fetcher's address.
2. The token is inside the seed, written to a root-only file, and reaches `node enroll` as a **path**,
   never in argv or in served user-data.
3. Consumption at `/enroll` is an atomic compare-and-set in the registry.

**The provisioning VLAN is a trust boundary** (5.6): anyone who can reach the boot service while a
host is armed and knows its hwid can race for the seed. Controls: the short window, arming only when
about to power the host, port security, and `status` showing who fetched; `arm --mac` pins the NIC.

After install, the late-command only writes files (the `kbf-deploy` binary verified against the
embedded root, the token file, a unit) and enables `kbf-firstboot.service`, a oneshot that retries
with backoff after network and time sync; enrollment happens there. An autoinstall failure runs
`error-commands` that POST the step and log tail to `/report/<hwid>` and **power off** (never
reboot). `status <name> --why` reads them. After expiry or a lost key, `enroll-token <name>` (or
`--pool`, `--all`) prints a fresh single-use token valid 1 hour.

### 4.7 The enrollment endpoint and Mac evidence

`POST /enroll` is on a second listener of `kbf-server`, the one unauthenticated surface in an mTLS
farm: bound to the provisioning and farm networks, rate-limited per source, one uniform 403 to any
failed evidence. It takes `{hwid, csr, evidence}`, then:

1. finds the record, which must be `approved`; the name is the record's;
2. verifies the evidence: a token (Linux) or the MDM identity (Mac);
3. applies the live-identity rule: a node already registered under another key is refused until
   `retire` or `replace`. **No automatic takeover.**
4. signs the CSR (one DNS name equal to the name, 30-day lifetime), audits the serial, returns the
   chain and sets the record `active`.

Renewal is the same endpoint over mTLS at half the lifetime (day 15), so a node survives 15 days cut
off; beyond that `enroll-token --all` recovers (9). NTP on every host; validation tolerates 5
minutes of skew and `status` flags more.

**Mac evidence.** The Mac enrolls in the MDM first; its SCEP identity carries the serial. At `/enroll`
it presents that certificate (mTLS), the server checks the serial equals the record's and asks
`kbf-mdm-gate` whether the device is still enrolled, **supervised** and not quarantined. A Mac not
bought through ABM can be unenrolled by its local admin and gets `trust = "test"`. A compromised MDM
is a compromised Mac fleet (it can push any package); the gate guards only the server's path to it.
Mitigations: the `.pkg` carries the farm's own signature, verified against the embedded root before
anything runs; ABM assignment alone is not approval (a serial the MDM sees but nobody approved stays
`pending` and gets no certificate); ABM list changes alert. The key lives in a root-owned System
keychain file; Secure Enclave storage is open.

### 4.8 Revocation and audit

`retire <name>` marks the record `retired`, **revokes the certificate by serial** (never by name or
hwid, so re-approving the same hwid does not revive it) and has the gate unenroll a Mac. The
revocation list is a signed, versioned object in the registry and object store, consulted by
`kbf-server` and envoy. Server rule: keep the last good list on disk; if the store is unreachable
keep using it and alert; refuse to start without one (fail closed at start, fail static after).
Nodes hold no store credentials, so retire needs nothing else to cut. Every register, approve,
enroll, renew, retire, arm, token mint, rename, name write, config publish and ABM change is appended
under `fleet/audit/<date>/` (object lock if supported) with the key id or policy rule id;
`status --audit` reads it.

## 5. Provisioning the operating system

### 5.1 Linux: DHCP to iPXE to the boot endpoint

```
power on -> UEFI boot order: local disk first; a blank or unbootable disk falls through to
  network boot (an installed host boots its disk every time and never gets here)
  -> UEFI PXE -> site DHCP (opt 66 = boot host, opt 67 = ipxe.efi) -> TFTP: the iPXE binary
  -> iPXE (embedded script + root) chains https://boot.example.net/boot?mac=..&uuid=..&serial=..
                                           &asset=..&buildarch=..
  -> endpoint decides (5.2): discover | install | local | rescue | wait
  -> install: installer fetches GET /seed/<hwid>, autoinstall, reboot
  -> kbf-firstboot.service: verify binary, enroll (4.7), node apply
```

- **PXE is for installs.** The firmware boot order is local disk first, network second. A new
  machine has a blank disk, so it falls through to PXE and the autoinstall puts Ubuntu on the boot
  drive; after that the machine boots that disk every time. The boot server can be down without
  affecting any running host. Reinstalling is explicit: a one-time network boot set through the BMC
  (for example `ipmitool chassis bootdev pxe`, which applies to the next boot only) or the firmware
  boot menu, together with `reinstall <name>` (4.6).
- The Ubuntu installer registers its own UEFI boot entry, which can change the order the operator
  set. The runbook verifies disk-first order once per machine after the first install (4.4, 5.6).
- The UniFi gateway hands every PXE client the same two values and **cannot vary the image by
  platform**; it does not need to, because the decision is in the HTTP endpoint. No proxyDHCP in the
  first cut.
- iPXE **embeds its chain script**: otherwise it DHCPs again, gets the same option 67 and loops. The
  operator's CI builds it (`boot ipxe-build`) with the fleet roots and boot URL embedded.
- TFTP serves only that one read-only file; everything else is HTTPS pinned to the root.
- One bootfile is one architecture: the Linux hosts are x86-64 UEFI; an arm64 Linux class needs its
  own VLAN or option 93 handling (17). Macs are not served here.

### 5.2 The boot endpoint

`GET /boot` is generic and knows no names or images. It looks the host up and applies the operator's
policy and image catalogue:

| Registry state | Answer |
|---|---|
| unknown hwid | record `pending`; serve **discovery** |
| `pending` | discovery again (it polls; the host sits at a prompt) |
| `approved`, armed, in window | the catalogue's image for this host, with its seed URL on the command line |
| `active`, not armed | `exit`: boot the local disk (reached only by a stray network boot; the endpoint mainly serves installs) |
| `active`, rescue requested (`rescue <name>`, one-shot, expires) | the catalogue's rescue image |
| `retired` or conflict | `exit` and an alert; never an install |
| endpoint unreachable | `exit` to local disk after a retry budget |

The **image catalogue** is the plug-in point. An entry is `{id, arch, kernel, initrd, cmdline
template, sha256s, seed template}`. kbf ships two providers: a directory of entries in the operator's
repository, and an executable hook given the record as JSON on stdin that returns an entry id (30 s
timeout; any error means install nothing, fail closed). The endpoint serves only entries whose
hashes it has verified, and the hashes come from the operator's config. Which class of host gets
which entry is a rule in `images.toml`.

### 5.3 The seed

Rendered per install from the record and the catalogue template: hostname = name; storage from the
approved `os_disk` and nothing else; network from the profile (DHCP; servers static); the apt
snapshot `kbf-updater` uses; the late-command and error-commands of 4.6. The installer needs
outbound access or an apt mirror. PXE plus autoinstall rather than an image means no image to rebuild
or drift; bootc remains the long-term target and replaces only the installer step. Without PXE,
`boot usb <name>` writes installer and seed for one armed host to a stick; it carries the token, so
it is one-shot and expires like any arming.

### 5.4 Macs: ABM and the MDM

1. The Mac is added in ABM under the farm's MDM; `mac assign <serial>` registers the serial with
   NanoDEP. No registry record exists yet.
2. Rack, ethernet (not Wi-Fi), power on. Setup Assistant is told the MDM and enrolls. **The first Mac
   needs a display and keyboard** (local admin, volume owner); later Macs may not. [A]
3. The MDM check-in creates the `pending` record (serial, model, memory, storage from
   DeviceInformation). The operator or policy approves; the naming policy gives the name.
4. At approval the MDM sets **DeviceName** and pushes the SCEP identity, the `kbf-mac-provision`
   profile and a signed, notarized `kbf-node.pkg`. Its postinstall installs a `launchd` job with
   `KeepAlive` and backoff that waits for the identity, runs `node enroll`, fetches the record,
   writes the daemon plist (flags, not environment) and loads it.

After `EraseDevice` and re-enrollment the serial maps to the same name; the live-identity rule still
applies. The MDM URL needs a public-CA certificate and the MDM host needs outbound to Apple (decision
4). Recovery without a console is `EraseDevice` through the MDM (operator-signed through the gate);
each Mac gets an HDMI dummy plug and the rack a KVM or cart display for when the MDM cannot reach it.

### 5.5 Servers

Servers follow 5.1 with a signed approval (4.6). `node apply` renders `kbf-server`, `kbf-ca`, MinIO
over the approved data disks (a rule such as "all non-OS NVMe", shown to the approver), envoy, boot
and MDM per `services`. A server is never reinstalled without `reinstall --server-confirm`, which
prints the disks it will not touch. One `voter` is accepted until HA lands; `check` fails on an even
count. Servers are never updated in parallel.

### 5.6 Prerequisites and BIOS baseline

- **Network:** a provisioning VLAN with only the farm's hosts; DHCP reservations for servers and
  options 66 and 67; DNS for the server, object and boot URLs; switch DHCP snooping and port
  security; NTP; outbound or an apt mirror during install; for Macs, outbound to Apple.
- **BIOS:** UEFI; boot order **local disk first, PXE over IPv4 second** (a blank disk falls through to PXE; an installed host never touches the network boot path; verify the order once per machine after the first install, because the Ubuntu installer registers its own boot entry and can reorder); restore-on-AC-power on;
  admin password; time set. **Secure Boot off** in the first cut (iPXE is not shim-signed;
  decision 6).
- **BMC:** on a separate management network; kbf uses it only to read the FRU name, and optionally write one back (4.4),
  in the first cut. Macs: ethernet, FileVault off, `autorestart 1`.

`doctor` checks reachability (DNS, NTP, server, boot service, Apple credential expiry) and is
separate from `check`, which is offline and deterministic.

## 6. Configuration

### 6.1 Facts are detected, not configured

Every connect reports and the registry records CPUs, memory, ISA and features, GPUs and drivers
(already on Hello); block devices by id with size, type and rotational flag, where the **SSD path
for cache and scratch** is the first mounted non-root SSD, else `/var/lib/kbf`, written back to the
node's status; NICs, BMC presence and the asset tag. The scheduler uses reported capabilities as
they are, so nothing typed can make a node look bigger than it is. `facts_history` keeps changes
and `status` flags drift (half the memory after a failed DIMM, a vanished drive). No config file
carries a CPU, memory or disk count.

### 6.2 Config is policy plus rare overrides

All of it lives in the operator's repository (7) and none is per host by default: profile rules
(which role and labels a class gets, for example `platform = macos` gets `pool = mac-arm`); the
naming, approval, disk and image rules; and **overrides** keyed by name and few (`reserve_cpus`, an
extra label, a `cache_dir` selected by mount or by-id only). An override never states a total or capacity (sizes are detected, 6.1), and `check` refuses one that does. `check` lists every override so none lingers. Order, lowest to
highest: built-in default, detection, profile rule, override; `render --explain <name>` shows each
value's origin.

### 6.3 How a node gets its configuration

Pull, not push, from two inputs. (1) The **fleet config**: policy, profiles, release pointers,
trusted roots, signed by an operator key and published by `apply` as one versioned object under
`fleet/config/`; the node checks signature, version monotonicity and key expiry, installs
atomically, and keeps the previous config if a new one is bad. (2) Its own **record**, served by
`kbf-server` over mTLS: name, role, services, labels, overrides. Role and services that grant power
carry the operator's approval signature, which the node verifies, so a lying server can misroute
work but cannot make a node a server or change its software. Object-store requests go through envoy,
which checks the certificate against roots and revocations and permits GET only on `fleet/config/`
and `fleet/releases/`; nodes hold no store credentials. Disclosure, accepted: the policy and release
manifest are readable by every node certificate, a record only by its node. Pull beats SSH because
the hosts converge without the operator's laptop online and Macs have no SSH by policy.

### 6.4 One render, three roles

`render <name>` shows the files a node runs, from its record and the config.

| Role | Linux | macOS |
|---|---|---|
| `server` | `kbf-server`, `kbf-ca`, MinIO, envoy, boot, MDM units as listed, nftables, chrony | none |
| `daemon` | `kbf-daemon.service` (rootless crun sandbox per action, daemon cache on), chrony | `com.kbf.daemon.plist` |
| `all-in-one` | `kbf-all-in-one.service` | its plist |

Every setting is a flag. A test parses every rendered command line with the real binary's parser, so
a rename fails the build; `--reserve-cpus` is rendered only for `daemon` and `all-in-one`.

## 7. The line between kbf and the operator's repository

kbf is a general build farm and holds no operator's specifics. **kbf ships mechanisms and schemas;
the operator's repository ships values.** Anything naming a machine, network, account or site is the
operator's. kbf's repository carries example values only (`example.net`, `build{n}`) and a lint
refuses real ones.

| In kbf (generic, public) | In the operator's repository (config files or hooks) |
|---|---|
| Registry, `/register`, `/enroll`, approval and arming state machine, audit | `approval.toml`: who or what may approve, which roles auto-approve, window lengths |
| Naming-policy engine and its schema | `naming.toml`: the rules (`build{n}`, `mac{n}`), counter starts |
| Boot endpoint, discovery image build, seed renderer, catalogue interface and its two providers | `images.toml` and the catalogue: OS release, apt snapshot, packages, admin SSH keys, which class gets which image |
| `boot ipxe-build` recipe | iPXE build inputs: embedded roots, boot URL, the CI job |
| Disk-selection rule engine | `disks.toml`: the rules (smallest NVMe is OS, other NVMe is MinIO) |
| MDM integration (gate, NanoHUB/NanoDEP/SCEP kit, DeviceName, pkg builder) | ABM and Apple accounts, DEP token and APNs certificate locations, MDM URL, Developer ID identity |
| FRU and asset-tag tool (`name write`) | BMC credential locations (a secret-store path, never the secret), BMC network, fields to write |
| CA, enrollment, revocation, rollout, node agent, `render`, CLI | CA locations (root and standby safes, offline-machine procedure), operator key roster |
| Config schemas, `check`, reference configs | `fleet.toml`: fleet name, server, object and boot URLs, root public halves; `profiles.toml`: pool labels by rule, reserve defaults; `overrides.toml`: the rare per-node entries; network notes (VLAN, DHCP option values) |

Two rules hold the line: kbf reads no path the operator did not pass by flag, and the schemas are
versioned and published, so the operator's repository and kbf change independently. For behaviour a
rule cannot express there is an executable hook (JSON on stdin and stdout, a timeout, fail closed),
which lives in the operator's repository. None of these files lists machines.

## 8. Updates

Two things change: the **fleet config** and the **software set**, both named by the signed config.

- Binaries: the config names a release signed by the release key. The node verifies signature and
  hash, installs to a versioned directory, switches a symlink, restarts and keeps the previous
  version. Linux OS packages come from the pinned apt snapshot via `kbf-updater`; macOS updates stay
  with the MDM.
- Rollout: `rollout <release> --pool linux-x86 --canary 1 --batch 4 --soak 30m`. Per node: cordon,
  drain (both exist), set the desired release, wait for Hello with the new version, run a qualify
  action, uncordon. A failed gate stops the rollout and restores the previous release on that node.
  The record is written to the object store before the first node is touched, so `--resume` works
  after an operator crash. The operator's machine still drives batches; the server-driven driver in
  fleet-updates.md replaces it later without changing the config.
- Servers: never in parallel; a voter only when the others are healthy; the leader last. Today:
  scp the binary and restart by SHA. A node that cannot reach the store runs its last verified
  release (fail static); a signature failure is an alert.

## 9. Cold start and disaster recovery

The boot service, CA, registry and object store share the three servers.

- **All down, disks intact:** power on; services start from local state; builders ran fail static
  and renew within 15 days of the outage. **Longer outage:** certificates expired; run
  `enroll-token --all` and deliver tokens, or `reinstall --pool`.
- **All three lost:** (1) root and standby come out of safe storage; (2) day 0 (15) rebuilds the
  first server from USB; (3) `nodes import` restores the last export (`backup pull`, or the copy in
  the object store): every record that was live lands `pending` (retired ones stay retired; never `active`, never `approved`), so no host
  gets a certificate without a fresh approval, matching the approved-before-`/enroll` rule (4.7).
  Discovery then reads each host's hand-written tag (SMBIOS or FRU asset tag, 4.4); a tag that
  matches an imported name starts a name claim, the operator confirms each claim (4.4, 4.6), and
  the naming counters re-seed from the highest claimed `{n}` of each template, so no live name is
  reissued. Hosts without a tag are named by policy at approval. (4) The CAS is a cache. Lost: cache contents and revocations after
  the last export (re-run `retire` for anything retired since).

## 10. The command set

`kbf-deploy` runs on Linux and macOS (a Mac needs only `node` and `hwid`). A test asserts `--help`
lists exactly these verbs.

| Group | Verbs |
|---|---|
| Registry | `nodes` (list, `show`, `--pending`, `export`, `import`), `approve` (`--name`, `--role`, `--all-pending`), `reject`, `rename`, `retire`, `replace`, `reinstall`, `rescue`, `enroll-token` |
| Config | `check`, `doctor`, `render [--explain]`, `apply`, `status` (`--why`, `--audit`, `--buckconfig`) |
| Updates | `rollout` (`--resume`, `--status`, `--batch`) |
| PKI | `ca init`, `ca sign`, `ca rotate`, `ca export-root`, `backup pull` |
| Provisioning | `boot serve`, `boot ipxe-build`, `boot discover-image`, `boot usb`, `name write`, `mac assign`, `mac unassign` |
| On the host | `node enroll`, `node apply`, `node renew`, `node doctor`, `hwid` |

The older kits fold in: `minio`, `envoy` and `mdm` become `services` rendered by `node apply`;
`linux render/check/install/swap/image-pull` become `render`, `check`, `node apply` and `rollout`;
`pilot` becomes `render` output and `status --buckconfig`. Old verbs stay as hidden aliases for one
release.

| Task | Commands |
|---|---|
| Add a Linux host | rack, cable (power, provisioning port, BMC), power on; `nodes --pending`; check serial and address against the box; `approve`; label the chassis with the printed name. An unrecognized host stays pending; a 409 alert means another machine took the seed (`reinstall`). |
| Add a server | `approve --role server` shows the OS and data disks the rules chose and waits for your token; `status` shows server healthy, MinIO online, voter joined before the next |
| Add a Mac | `mac assign <serial>`; power on (the first needs a display and keyboard); `approve <serial>`. Stuck in Setup Assistant: not ADE-assigned, fix ABM, erase through the MDM |
| Remove, replace, rebuild | `retire <name>`; `replace <name>`, power on the new board, `approve`; `reinstall <name>` (set a one-time network boot first, `ipmitool chassis bootdev pxe` over the BMC or the firmware boot menu, then power cycle; the bootdev applies to the next boot only) |
| Change a label or `reserve_cpus`, roll a release | edit `overrides.toml` or a profile rule, `apply`; `rollout <release> --pool ...` |
| Replace a MinIO drive | swap it; `node doctor` prints the heal command and waits; never two drives on one node |

## 11. What is retired

`install.sh`, `install_units.sh`: replaced by `node apply`. The operator's `deploy/` shell kits:
logic moves into `kbf-deploy`; the repo keeps the section 7 files. `fleet.toml` as a list of hosts:
replaced by the registry; the operator's `fleet.toml` is
fleet-level values only. `pilot_ca.sh` and the deny-list file: replaced by `ca`, `/enroll`,
revocation. Buildbarn-only and cutover scripts, dev-farm scripts (`kbf_dev_install.sh`,
`upd_main.sh`; the dev farm becomes a second registry) and hardcoded node numbers, subuid ranges and
commit SHAs. Each is removed in the change that makes its replacement work, never before.

## 12. What is still manual

Racking, cabling, labeling, BIOS, switch, VLAN and DHCP options; the day-0 ceremony (15); **approving**
each new machine against what was racked (unless a policy does, 4.6); ABM serial assignment, non-ABM
Mac enrollment and the first Mac's display and keyboard; drive replacement and power cycles of hosts
without a BMC. Recurring, with the failure when it lapses: ABM DEP token (yearly; new Macs do not
enroll); APNs certificate (yearly; the MDM cannot reach any Mac; renew under the same account);
Developer ID (no new `.pkg`); intermediates and operator keys (yearly; no enrollments or config
publishes). `doctor` warns 60 and 30 days ahead.

## 13. Tests

- Registry: unknown hwid becomes `pending`; duplicate hwid refused; restart keeps arming, tokens and
  revocations; import never overwrites a live record; 10 000 synthetic records page and index in
  bounded time.
- Naming: first match wins; counters atomic under concurrent approvals; retired names not reissued;
  no rule leaves the host pending; a tag naming another hwid is a conflict.
- Policy: auto-approval never approves the server role; a server approval without the operator
  signature is refused by the registry.
- Boot endpoint: unknown gets discovery; unarmed `active` gets `exit`; armed gets the image once and
  the seed once (409 after); `retired` never installs; a hook that errors installs nothing; the seed
  never touches a disk other than the approved `os_disk`.
- Detection: changed facts recorded and flagged; the scheduler uses reported capabilities only.
- Enrollment with the fake CA: wrong hwid, reused token, expired token, token in argv, live-identity
  conflict, revoked serial, retire then re-approve (old certificate stays dead), expired operator
  key; each must fail. `name write` against a fake BMC: a refused write keeps the registry name and
  sets `tag: unwritten`.
- Golden renders with real-parser command lines; first-boot retry; rollout resume without
  re-draining; rollback refused; the section 10 table equals the command tree; the kbf repo lint
  refuses real values in shipped config.
- A planted mutant per area (skipped signature check, token consumed twice, name allocated twice)
  must turn its test red. Per hardware model: iPXE uuid equals `product_uuid` (4.3); per BMC model:
  the FRU write check (4.4).

## 14. Migration from today's hosts

Each step reversible. (0) Ship the durable registry; nothing below enables `/enroll` before it.
(1) Ship `/register` and `nodes`: existing hosts self-report with `node enroll --register-only` (a
human on the host is the proof) and the operator approves with the **current** node id as the name,
so no certificate or flag changes yet. (2) Ship `render` and diff against live units until empty.
(3) Ship `/enroll`, `ca`, `node enroll`; enroll in place with `enroll-token`; the daemon switches at
its next restart; the old shell CA stays valid until all are enrolled. (4) Ship the Linux node agent,
`apply` and `rollout`; flip one pool at a time, canary first. (5) Ship the boot service and `name
write`: the next new Linux host is the first user, the operator hand-writes existing hosts' names into the asset tag at the next maintenance window (4.4), and no existing host is reinstalled for it. (6) Macs are **blocked on the macOS node
agent**; until then they keep the current install, then ABM serials, approval, and the pkg in place,
verified on one Mac first. Servers last; the single server moves to `kbf-deploy` rendering before HA,
so HA is a registry change.

## 15. Day 0: building the farm from nothing

Steps 1 to 5 need no object store. (1) Prerequisites (5.6); Apple accounts and a public-CA
certificate. (2) Root ceremony on an air-gapped machine: `ca init` makes root and standby, each into
its own safe. (3) Write the operator's config files (7) plus a tiny `nodes.json` naming the first
server (the one bootstrap use of `nodes import`); `apply --to-dir spec/`. (4) First server from USB:
`boot usb srv-1 --spec-dir spec/`; its first boot writes an intermediate request, carried to the
offline machine for `ca sign`. (5) `kbf-server`, `kbf-ca` and `boot serve --spec-dir` run on srv-1.
(6) Power on srv-2 and srv-3: they appear pending, you approve each as a server with your token, they
install over PXE from srv-1 and get their own intermediates. (7) MinIO forms, envoy starts, `apply`
publishes to the object store and boot switches to `--config-url`. (8) MDM, ABM, DEP token, push
certificate. (9) Linux builders, then Macs.

## 16. Decisions and questions for the owner

1. **Registry as source of truth, self-enrollment, manual approval by default.** Recommended: yes;
   policy auto-approval stays off until you ask.
2. **Naming-policy defaults.** Recommended: one rule per hardware class (4.5), counters from 1,
   retired names never reused, operator override at approval. Question: do servers share the scheme
   or take a role prefix (`srv1`)?
3. **Names are hand-written asset tags.** Decided by the owner: the operator writes each machine's
   name into the SMBIOS chassis asset tag and/or BMC FRU asset tag at first boot (4.4). FRU
   writability no longer blocks anything: the BIOS tag can always be set by hand, and FRU write via
   `ipmitool` is a convenience.
4. **ABM and MDM hosting.** Recommended: accept the Apple dependency (ABM, DEP token, APNs,
   Developer ID, public-CA certificate); MDM on a server host if the farm network reaches Apple.
5. **Where the CA key lives.** Recommended: a `kbf-ca` process per server host (own user,
   name-constrained, TPM-sealed where present), accepting in writing that root on a server mints
   fleet-name certificates until its intermediate is revoked.
6. **Secure Boot.** Recommended: off first, BIOS password on, signed shim and grub later.
7. **Server-role approval needs the operator's hardware token.** Recommended: yes.
8. **TPM attestation.** Recommended: not first cut; the spoof risk (4.3) is documented.
9. **Pull-based config; 30-day certificates renewed at half-life.** Recommended: yes.
10. **Operator repository layout.** Recommended: the section 7 files in one directory of the
    operator's repository, schemas and examples in kbf.

## 17. Open items

Secure Enclave storage of the Mac node key; switch-port or LLDP mapping to rack slots (feeds naming
rules); TPM-backed enrollment; the macOS node agent and the CI build of iPXE; an arm64 Linux class
(option 93 through proxyDHCP, or a second VLAN); automating the one-time BMC network boot and power cycle (Redfish or
IPMI) so `reinstall` needs no person; unattended Mac enrollment on the newest macOS, MDM
reachability and the 15-minute install figure ([A] until measured); server HA depends on ha.md.

## 18. Review record

Every must-fix from the two earlier reviews was taken, with these changes: the per-node spec slice
is gone (nodes read one signed common config and their own record, 6.3); no separate signer machine
(decision 5); BMC use is limited to the FRU name; per-operator delegated keys, not CI, sign config;
TPM attestation and Secure Enclave keys are deferred with the risk stated. On the owner's direction the
inventory file as source of truth is replaced by the registry; keys, certificates, revocation,
one-shot arming and rollout carry over.
