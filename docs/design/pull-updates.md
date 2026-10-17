# Pull-based updates: hosts update themselves over the session they already hold

Status: design; the wire contract (section 5) is frozen, the rest is built in the units of section 11.
Adapted from the pull-update design study; labels: **[F]** read in code at the time of writing,
**[D]** a design decision, **[V]** verify before building on it.

Related: [fleet-updates.md](fleet-updates.md) and [fleet-updates-security.md](fleet-updates-security.md) (the
earlier push-shaped design this one cuts down), [architecture-v2.md](architecture-v2.md) (sections 4 and 11: the
state store, protocol skew, upgrade order), [worker-protocol.md](worker-protocol.md), [../api.md](../api.md).

## 1. The answer in ten lines

1. An operator (or an auto-update rule the operator set) names a **desired release**: one commit of this
   repository's `main`, whose build attestations are on GitHub. The frontend stores it.
2. Every host learns the desired release over the **mTLS session it already holds** with the frontend: an
   `UpdateOffer` sent after `Welcome` and at every change. No new inbound port, no SSH, no CI credential that
   reaches a host.
3. The host's unprivileged `kbf` process fetches the tarball from the frontend over the same listener
   (`ReleaseService.Fetch`), drops it in an inbox directory, and reports `staged`.
4. A small root helper (no socket, no network) **verifies the build attestation offline** with a pinned `gh` and a
   pinned trusted root, and installs into a new versioned directory. It never overwrites a running binary.
5. The **frontend alone orders** the rollout: it cordons a host, waits for zero leases (and, on storage nodes, for
   `under_replicated_blobs == 0`), then sends `UpdateOrder{ACTIVATE}`. The helper flips `current` and restarts.
6. The new process registers; the frontend checks health and sends `UpdateOrder{CONFIRM}`. If confirmation does
   not arrive in the window, or the unit crash-loops, the helper reverts locally (commit-confirm).
7. Order: canary host, storage-and-daemon nodes one at a time, other Linux daemon hosts, the Macs, the frontend last
   through its own drain.
8. Rollback is setting the desired release back: a **higher generation** naming an older commit. The old
   directory is still on disk, so nothing is downloaded.
9. Only the unprivileged `kbf` binary is ever pulled. The helper, unit files, sudoers entry and the pinned trust
   material change only by a script a human runs. Root code is never auto-updated.
10. The operator sees every host's version, an "update available" notification, a button to update, and an
    optional auto-update rule. All-in-one is the same button on one machine.

## 2. Trust model (MVP decision)

**Hosts trust the GitHub build attestation of this repository's `artifacts` workflow for `refs/heads/main`,
verified offline on the host** with a pinned `gh` and a pinned `trusted_root.jsonl`, exactly as the scoped
installer for the development nodes does it: `gh attestation verify <tarball> --bundle <attestation>
--custom-trusted-root <pinned trusted root> --repo <owner/repo> --signer-workflow
<owner/repo>/.github/workflows/artifacts.yml --cert-oidc-issuer <GitHub Actions issuer> --source-ref
refs/heads/main --source-digest <the offered commit> --predicate-type <build provenance>`, with no network. The
commit the attestation names must be the commit the offer names.

The frontend **only stores and distributes** the desired commit and the release bytes. It is **not trusted for
integrity**: a host re-checks the tarball's SHA-256 against the attestation's subject digest and the attestation
against the pinned root, and installs nothing else. The human approval is the click (or the auto-update rule the
operator set).

| Actor | Can do | Cannot do |
|---|---|---|
| GitHub / CI | build, attest, publish | reach a host, push to the frontend |
| Compromised **frontend** | choose when and in what order; withhold; offer any older attested build; stage files; deny service | make a host install a binary that lacks a valid `main` attestation of this workflow; run anything as root |
| Compromised **service user** (or an action under the native driver) | write the inbox; stage an attested release; order an activation early | install an unattested binary; touch the versions directory, the helper or the units |
| Anyone who can push to `main` | get an attested build | (see residuals) |

Residuals, stated plainly:

- **`main` has no branch protection.** Anyone who can push to it gets an attested build, and so a binary every
  host accepts when the frontend offers it. The attestation proves *where a binary was built*, not *that a
  reviewer approved the source*.
- **A hostile frontend can offer any older attested build.** There is no downgrade floor in the MVP; the
  generation only orders offers within one frontend. A rollback to an older commit is deliberately possible, so
  the host cannot refuse "older" without an operator-set floor.
- **The binary holds credentials.** On a storage-and-daemon host the `kbf` process holds the cold-store
  credential, so the set of commits that can be attested is the set of programs that can read it.
- **The design's separate Ed25519 release key is deferred.** A release key held off CI and off the shared session
  account would close the first two residuals (a human signs; a floor can be signed in). It stays a documented
  later option: the offer would grow a signed manifest field, and the helper would check it before the
  attestation. Nothing in the MVP wire prevents adding it.

## 3. What exists (and where this builds on it)

- **Artifacts** [F]: one `kbf-<commit>-<platform>.tar.gz` per platform holding one binary, `SHA256SUMS`, SBOMs,
  and a build-provenance attestation per tarball, on pushes to `main`. `kbf --version --json` prints `{commit,
  min_protocol, protocol, profile, state_schema, version}`. A release is verified, not rebuilt.
- **Versioned host layout** [F]: `/usr/local/kbf/releases/<commit>/`, `current` and `previous` symlinks, a
  `state.json` with a pending confirm (and an order-floor field kept only so an older state file reads); `kbf-deploy host install|rollback|gc`. The units run
  `<KBF_BIN_DIR>/kbf <mode>`, so pointing `KBF_BIN_DIR` at `current` needs no new unit shape.
- **Rollout state machine** [F]: a blocking engine (gate, stage, drain, wait idle, swap, wait healthy, wait
  repaired, confirm, undrain; rollback on failure; pause; fleet lock) over a `Transport` trait, with a fake fleet
  and planted-fault tests. It had no real `Transport`; this design supplies one over sessions.
- **The session** [F]: `kbf.worker.v1.Worker.Session`, `Welcome` (with the lease `epoch`), `MapUpdate` pushes,
  `NodeStatus` sent once after `Welcome`. The server keeps the newest per-node status from the node's current
  stream only.
- **The frontend** [F]: the operator API (`/v1/nodes`, node cordon/drain/uncordon, `/v1/frontend/drain`), a state
  store (`kbf-state`: compare-and-set rows C1, monotonic counters C3), and a drain that ends in a process exit.
- **Not built, and failing closed until it is** [F]: `under_replicated_blobs` (a planned metric of
  architecture-v2 section 11). The storage gate of section 8 refuses to order a storage node while the metric is
  absent.

## 4. Components

```text
 operator / UI               frontend host                              every other host
 -------------               -------------                              ----------------
 /v1/update, /v1/releases -> kbf frontend                               kbf (daemon / storage-and-daemon)
 (click, auto rule)            release store (--releases-dir)             update client (kbf-node)
                               desired release, rules (state store)         | inbox dir (service user writes)
                               offers + orders over sessions                v
                               rollout controller                         helper (root, no socket)
                                                                            versioned dirs, commit-confirm
```

| Piece | Where | Notes |
|---|---|---|
| Wire messages | `crates/kbf-proto/proto/kbf/update/v1/update.proto` | messages and one service; frozen (section 5) |
| Platform names | `kbf_types::ReleasePlatform` | `linux-x86_64`, `linux-arm64`, `darwin-arm64`; from a node's `os` and `arch` report entries |
| Release store, desired release, offers, `ReleaseService` | `kbf-server` | section 6 |
| Update client | `kbf-node` | fetch, inbox, status, refuse an order while a lease is live |
| Helper (verify offline, install, flip, revert) | a root unit on each host, one shared installer library | verbs in section 7 |
| Rollout controller | `kbf-server`, replaces the older record in `rollout.rs` | section 8 |
| Release discovery | the frontend polls GitHub, read-only | proposes a desired release; never applies |
| Fleet view | the UI, over `/v1/nodes` and `/v1/update` | section 9 |

## 5. The wire contract (frozen)

`kbf.update.v1`, in `crates/kbf-proto/proto/kbf/update/v1/update.proto`, is the contract; this section states the
rules the field comments cannot. Numbers in `kbf.worker.v1`: `DaemonMessage.update_status = 7`,
`ServerMessage.update_offer = 8`, `ServerMessage.update_order = 9`. `UpdateStatus` is its own message because
`NodeStatus` is sent once per `Welcome` and an update changes more often.

**Component.** One tarball, `<name>-<commit>-<platform>.tar.gz`, holding exactly one regular file named `<name>`.
`name` is `kbf` or `kbf-daemon`. `sha256` is 64 lowercase hex digits of the tarball and is also its name in
`ReleaseService.Fetch`. `attestation` is the build-provenance bundle (JSON lines, at most 256 KiB) of the tarball.

**Which components a platform needs.** `linux-x86_64`, `linux-arm64`: `kbf`. `darwin-arm64`: `kbf-daemon`. A
platform is **complete** for a commit when the frontend holds every component it needs, with their attestations.

**UpdateOffer.**
- `generation` strictly increases at each change of the desired release, survives a frontend restart, and a
  rollback to an older commit gets a *higher* generation than the release it replaces. Setting the commit that is
  already desired changes nothing and keeps the generation.
- `platform` is the node's own, derived by the frontend from the node's `os` (`linux`, `macos`) and `arch`
  (`x86_64`, `arm64`) report entries. A node with any other pair gets no offer.
- The offer is **level-triggered**: sent after `Welcome`, and to every connected node at every change. A node that
  connects later gets the current offer too. A node whose platform is not complete gets none and is shown
  `unavailable`.
- An offer with the generation the node already holds is a repeat (for example the platform became complete after
  the change). A node acts on a (generation, commit) pair once.

**UpdateOrder.** `order_id` comes from a store counter and strictly increases within an `epoch`; `epoch` is the
`Welcome` epoch, which moves only when a store is restored (a handover keeps it). Orders are ordered by **(epoch, then
id)**, by the node's client and again by the helper, which acts on each pair once: a higher epoch is newer whatever its
id (a restored store counts ids again), an older epoch is refused whatever its id, the same epoch with a higher id is
newer however large the gap (the counter is the store's, not the host's), and the same pair again is a replay. There is
no floor on the installer's side: a rollback is the old release ordered again, and the order of orders is the
client's and the helper's alone. The helper names an activation `<epoch>-<id>`, so the same id in two epochs is two
activations. An epoch more than 1000 above the last acted on is refused with the reason in `status.json` and not
acknowledged, so a mistaken or hostile huge epoch cannot put every real order below the floor; the first order a host
acts on is measured from nothing and is taken at any epoch. A compromised service user can still write a first order
with a huge epoch: an availability limit, the same exposure as ordering an activation early (section 7.3 of the
design study). `window_s` is the confirm window of `ACTIVATE`; the helper clamps it to its own
limits (default 15 minutes, range 2 to 30), so a compromised frontend cannot set a year. The window starts when the
new process starts, not when the link flips.

**UpdateStatus.** Sent after `Welcome` and on every change. `running_commit` is the process's own build commit
(12 hex or `unknown`); `installed_commit` and `staged_commit` are 40 hex from the helper, empty if not known.
`acked_order_id` is the newest order acted on; `offered_generation` the newest offer seen. A node that never sends
it is listed `unsupported`, and a rollout halts at it (it never skips a host silently).

**ReleaseService.Fetch.** On the worker listener, admitted nodes only (mutual TLS). The argument is 64 lowercase
hex digits and nothing else; an unknown digest is `NOT_FOUND`. The server never builds a path from anything but the
validated digest. Chunks are at most 1 MiB.

**Compatibility.** A server that predates these fields ignores `update_status`; a node that predates them ignores
the offers and orders, never sends a status, and is listed `unsupported`.

### Contracts owned by the node client

The inbox directory the unprivileged `kbf` writes and the root helper reads, and the world-readable `status.json`
the helper writes, are specified by the update-client unit and frozen with it. This document does not restate them
so that there is one source; they are linked here when that unit lands.

## 6. The frontend

### 6.1 Flags and failure

`--releases-dir DIR` names the content-addressed release store (files named by their SHA-256). Every update
endpoint needs it **and** a state store (`--state-store`). Without either, every update endpoint answers **503**
saying which is missing; nothing answers an empty 200.

### 6.2 State store rows

| key | value |
|---|---|
| `update/release/<commit>/<platform>/<name>` | the component: `sha256`, `size`, and the attestation once uploaded |
| `update/desired` | the desired release: `generation`, `commit`, `set_at` |
| `update/generation` | the counter (C3) the generations come from |
| `update/auto` | the auto-update rule `{enabled}` |

### 6.3 Operator API (writes need the existing token gate)

- `PUT /v1/releases/{commit}/{platform}/{name}`: the tarball as the body, streamed to a `.partial` file with the
  SHA-256 computed while streaming (size cap 512 MiB), fsynced, then renamed to `<sha256>`; the answer carries the
  SHA-256. `PUT .../{name}.attestation`: the bundle (at most 256 KiB). The same (commit, platform, name) with
  different bytes is **409**: a release is immutable. The frontend also checks the tarball is gzip+tar holding
  exactly one regular file named `<name>`; that is an early failure for mistakes, not trust. Path segments are
  validated (40 lowercase hex, a known platform, a known name) and never joined raw.
- `GET /v1/releases`: commits with, per platform, which components are present and whether the platform is complete.
- `POST /v1/update/desired {commit}`: the commit needs at least one complete platform (else 409). Compare-and-set
  on `update/desired`; the counter gives the new generation only when the commit differs from the current one.
- `GET /v1/update`: `{desired, auto_update, releases}`. `POST /v1/update/auto {enabled}` stores the rule.
- `GET /v1/nodes` gains, per node, an `update` object: state, the three commits, offered generation, refusal,
  `helper_present`, `up_to_date` (true, false or null) and `availability` (`ok`, `unavailable`, `unsupported`).

### 6.4 Sessions

After `Welcome`, and to every connected node at every change of the desired release or of a platform's
completeness, the frontend sends an `UpdateOffer` for that node's platform. `UpdateStatus` is kept as the newest per
node and ignored from a replaced stream, exactly as `NodeStatus` is.

## 7. The host

```text
/usr/local/kbf/releases/<commit>/kbf          root:root 0755, never modified after install
/usr/local/kbf/current   -> releases/<commit> the running one (units use KBF_BIN_DIR=/usr/local/kbf/current)
/usr/local/kbf/previous  -> releases/<commit>
```

Retention: the running, the previous and one more directory.

Helper verbs, fixed, no free-form arguments (the host's own config names the unit; nothing a request carries is a
path or a command):

| Verb | Does |
|---|---|
| `stage` | verify the tarball's SHA-256 and its attestation offline; check the tarball holds one file `<name>` and `--version --json` of it names the offered commit (run as the service user, empty environment); install into `releases/<commit>/`; do not activate |
| `activate` | write `pending_confirm` first (fsync, rename, directory sync); flip `previous` then `current` (atomic symlink rename); restart the one configured unit |
| `confirm` | cancel the revert timer; record `last_good` |
| `revert` | the timer, a crash-loop counter, or an order: flip `current` back to `previous`; restart |

- The helper **never executes the new binary as root**.
- Order of writes: `pending_confirm` before the flip, the flip before the restart; on start the helper reconciles
  its state against the links, resolving to the state that keeps `previous` intact.
- Linux: an `ExecStartPre` boot check decrements a start counter while `current` is unconfirmed, so a crash loop
  reverts in seconds with no timer. macOS: a LaunchDaemon; the revert timer is the only trigger.
- macOS: a staged file that carries `com.apple.quarantine` is refused; the binary must verify with `codesign
  --verify --strict` and carry the CI ad-hoc signature (`adhoc,runtime`).

## 8. The rollout

The frontend alone decides order. It holds one **update slot** in the state store (a compare-and-set row, no
time to live: if the holder dies the slot stays held and the run goes `held`; a broken host never lets a second one
start).

1. **Order**: canary host, then storage-and-daemon nodes one at a time, then the other Linux daemon hosts, the
   Macs, the frontend last.
2. **Per host**: cordon (no new leases), wait for zero leases (a storage node also waits for
   `under_replicated_blobs == 0` and for reads to be served by the other owner), send `UpdateOrder{ACTIVATE}`,
   check health (registered at the target commit, serving, storage repaired), send `UpdateOrder{CONFIRM}`,
   uncordon, next host.
3. **Failure**: an error after the swap rolls back, never retries. A host reverted by its own timer reports
   `REVERTED`; the run goes `held` and stays held until a person acts.
4. **Pause** stops granting between hosts, never mid-host. **Rollback** is setting the desired release to the older
   commit (same gates, same order, no download).
5. **Protocol skew** (architecture-v2 section 11): the planner takes the frontend's position from the release's
   `protocol` and `min_protocol`, as `kbf --version --json` declares them: frontend last when the new protocol is
   not above the frontend's; frontend first when it is, provided the new `min_protocol` admits the oldest running
   daemon; otherwise it refuses and says it is a flag day. A release with a `state_schema` above the frontend's
   goes daemons-first.
6. **The frontend updates itself last**: stage as on any host, wait for an idle window (`open_executions == 0`),
   activate, then drain (`POST /v1/frontend/drain`): new Executes are refused, open ones finish, the process exits
   0 and systemd restarts it into the new binary. With socket activation (`LISTEN_FDS`) the listening sockets
   survive the restart, daemons keep running their actions, the SQLite state store keeps the queue and leases, and
   daemons reconnect and re-bind their leases. If `/readyz` is not true in the window, the helper reverts.
   Zero-impact frontend updates need two frontends on a shared database and are later.

## 9. The fleet view

Every host's running and installed version (`GET /v1/nodes`), an **update available** notification (the frontend
polls GitHub, read-only, for the newest attested artifacts run on `main` and proposes it as a release), a **button**
that sets the desired release, and an optional **auto-update rule** (`POST /v1/update/auto`). The frontend applies a
release only on the click or the rule. All-in-one is the same button on one machine.

## 10. Failure modes and their tests

Each safety property has a planted mutant that turns its test red.

- **Wrong bits**: a flipped byte of a tarball is refused at the hash; an attestation for another digest, repo,
  workflow or ref is refused; an inbox entry that is a symlink, a FIFO or oversized is refused with no read outside
  the inbox.
- **Capacity**: two hosts ordered at once (the slot admits one); storage under-replicated or the metric absent (no
  order); a host with live leases (order withheld, and the node refuses it too).
- **Bad binary**: starts and exits at once (the counter reverts, Linux); runs but never registers (the window
  expires, the host reverts, the run goes `held`); the frontend dies in the window (the host reverts); `previous`
  missing (the host stays, reports `REFUSED`).
- **Crash story**: the helper killed at each step of `activate` converges to the old or the new state, never a
  mix; power loss between rename and directory sync leaves one whole state.
- **Frontend**: a restart after an order resumes from the host's recorded phase and does not stage again; a replayed
  order id is a no-op and a lower epoch is refused.
- **Store**: the release store is immutable per (commit, platform, name); a crash between writing and renaming
  leaves no component; traversal segments touch nothing outside the store.

## 11. Units

| Unit | What |
|---|---|
| wire and store (this contract) | `update.proto`, `ReleasePlatform`, the release store, the desired release, `/v1/update`, `/v1/releases`, update view in `/v1/nodes`, offers over the session, `ReleaseService.Fetch` |
| node client | fetch, inbox, status reporting, refuse with live leases, relay of orders |
| installer core | versioned directories, offline attestation check, commit-confirm, revert, retention |
| release source | the frontend's read-only poll of GitHub for the newest attested `main` artifacts |
| rollout controller | the slot, the order, the gates, resume, pause, frontend self-update |
| fleet view | the UI page and the auto-update rule |
| bootstrap and rehearsal | the host-side scripts a human runs once per host; rehearsal on development nodes |

## 12. Deferred

- The Ed25519 release key and a signed floor (section 2).
- Sigstore verification beyond the attested-`main` rule (a protected `main`, required reviews).
- A frontend-signed grant on each order, closing the early-order gap of an inbox writer under the native driver.
- Maintenance windows, percentage waves, tracks beyond canary plus stable.
- Two frontends on a shared database (zero-impact frontend update).
- OS, Xcode, VM-image and MDM updates ([fleet-updates.md](fleet-updates.md) owns those).
- Updating the helper, the units, the sudoers entry and the pinned trust material: manual by design.
