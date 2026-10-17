# The crate layout: a map of the workspace and a proposal to simplify it

Status: proposal. Nothing in this document has moved yet; every move it proposes waits for
a decision, and then lands as one of the small pull requests in
[the step order](#4-the-step-order). Every "today" statement describes `main` at
`00b78c50` (#627); a count is what the command named beside it printed there.

A newcomer who opens `crates/` sees 40 directories, all but one named `kbf-*`, and not one
`Cargo.toml` with a `description`. Nothing on that page says which two of them make the
farm, which ones only run on a Mac, and which only exist for tests. This document maps
every crate (section 1), proposes fewer and better-named crates in groups (section 2),
writes down the path a new user takes to a working single-node farm (section 3), and orders
the work (section 4).

## 1. The crate map

`ls crates` prints 40 directories: 39 `kbf-*` crates and `xtask`. Every one sets
`publish = false` (the workspace default, or its own line for `kbf-testkit` and `xtask`),
so no crate is on crates.io and no name is taken there.

Of the 40, 38 have a library target (all but `kbf-node` and `kbf-vmm`). They define 21
binary targets: 9 that a farm or its operator runs, 8 that only tests run, 2 that only CI
runs, one stub and one agent whose driver is not built yet (the table below names each).
Lines are `wc -l` over the crate's `.rs` files, tests included.

**Audience** says who ever runs or reads the crate:
- **user**: someone running a farm runs this program;
- **operator, optional**: a program for a larger or measured farm, not needed for one node;
- **Mac**: only for Mac workers (built and tested on Linux too, where it can be);
- **library**: linked into a program; a user never names it;
- **test** / **CI**: only tests or CI build or run it;
- **planned**: built and tested, but nothing ships it or calls it yet.

| Crate | Lines | Targets | What it is | Depended on by (normal; *dev*) | Audience |
|---|---:|---|---|---|---|
| `kbf-server` | 28823 | lib, bin `kbf-server` | the farm server: REAPI for clients, `kbf.worker.v1` for daemons, the operator API; wires the crates below | kbf-sim-cell; *kbf-bench, kbf-daemon, kbf-it, kbf-node* | user |
| `kbf-node` | 3968 | bin `kbf-daemon`, bin `kbf-podman-stub` (test) | the worker daemon's binary: flags and the `--driver` choice | none | user |
| `kbf-daemon` | 17297 | lib, bin `kbf-keeper-swap` (test) | the worker daemon's library: session loop, lease manager, the `Runtime` trait, the input cache | kbf-bench, kbf-driver-container, kbf-driver-native, kbf-it, kbf-node, kbf-sim-cell | library |
| `kbf-driver-container` | 15293 | lib, bin `kbf-fake-podman` (test) | the Linux execution driver: each action in a fresh rootless Podman container | kbf-node (Linux only) | library |
| `kbf-driver-native` | 21226 | lib | the plain-process driver for Macs, and the whole-machine driver through `kbf-mac-session` | kbf-node | library, Mac |
| `kbf-front` | 18037 | lib | the REAPI services (CAS, ByteStream, action cache, Execution) over a `Cache` and a `Dispatch` | kbf-server, kbf-sim-cell; *kbf-bench, kbf-daemon, kbf-driver-container, kbf-it, kbf-node, kbf-transfer* | library |
| `kbf-auth` | 1764 | lib | REAPI authentication and authorization layers (tower) | kbf-front, kbf-server | library |
| `kbf-sched` | 20854 | lib | the scheduler as a pure state machine: operations, leases, fencing, placement | kbf-front, kbf-invocation, kbf-log, kbf-server, kbf-sim-cell | library (pure) |
| `kbf-meta` | 3385 | lib | the metadata state machine: CAS index, action cache, closure check | kbf-front, kbf-log, kbf-server, kbf-sim-cell, kbf-tier | library (pure) |
| `kbf-invocation` | 5695 | lib | the per-build invocation record, built from events | kbf-front, kbf-server | library (pure) |
| `kbf-raft` | 4036 | lib | a sans-IO Raft core and its host loop | kbf-server, kbf-sim-cell, kbf-store | library (pure) |
| `kbf-store` | 3191 | lib | a Raft server's durable state on local disk (log segments, hard state, snapshot) | kbf-front, kbf-server, kbf-sim-cell, kbf-tier | library |
| `kbf-log` | 3975 | lib | the replicated log's command encoding to and from `kbf.log.v1` | kbf-server, kbf-sim-cell | library |
| `kbf-tier` | 3401 | lib | the local write tier's disk layer (block files, budget, restart) | kbf-front, kbf-server | library |
| `kbf-segments` | 1200 | lib | the segment format that packs blobs into objects, and FastCDC chunking | kbf-front, kbf-server, kbf-tier, kbf-transfer | library (pure) |
| `kbf-objstore` | 2766 | lib | the `ObjectStore` trait, an in-memory and an S3 store, the conformance suite | kbf-front, kbf-server; *kbf-daemon, kbf-it, kbf-transfer* | library |
| `kbf-transfer` | 5551 | lib | the daemon's one blob transfer engine (one connection, one in-flight limit) | kbf-bench, kbf-daemon, kbf-driver-container, kbf-it, kbf-node | library |
| `kbf-outputs` | 2689 | lib | an action's outputs read without following symlinks, and the lease directory removed | kbf-driver-native, kbf-node | library |
| `kbf-caps` | 3118 | lib | CPU and GPU capability parsing, ISA levels, request matching | kbf-daemon, kbf-front, kbf-log, kbf-sched, kbf-server, kbf-sim-cell | library (pure) |
| `kbf-types` | 2479 | lib | shared value types: digests, platforms, leases, farm time, `StateMachine` | 17 crates (kbf-raft as a dev-dependency) | library (pure) |
| `kbf-proto` | 504 | lib | generated code: REAPI v2, `kbf.worker.v1`, `kbf.log.v1`, gRPC health | 11 crates (kbf-it and kbf-node as dev-dependencies) | library |
| `kbf-deploy` | 27487 | lib, bin `kbf-deploy` | the deploy tool: kits `linux`, `minio`, `envoy`, `pilot`, `mdm`, each a dry-run-first plan | *kbf-it, kbf-node* | operator, optional |
| `kbf-host` | 3084 | lib | plan-then-apply host changes shared by `kbf-deploy` and `kbf-updater` | kbf-deploy, kbf-updater | library |
| `kbf-updater` | 4153 | lib, bin `kbf-updater` | the root helper that verifies and installs signed software sets on a node (Linux) | none | operator, optional |
| `kbf-mdm` | 7348 | lib, bin `kbf-mdm-gate` | the MDM gate: the only holder of the Mac MDM's key, over mutual TLS | kbf-deploy | Mac |
| `kbf-mdm-api` | 1070 | lib | generated `kbf.mdmgate.v1` gRPC, and the names server and gate both check | kbf-server | Mac |
| `kbf-mac-session` | 15993 | lib, bin `kbf-mac-session` | the Mac's root helper: a throwaway user per lease, GUI session switch, boot reset | kbf-driver-native; *kbf-mdm* | Mac |
| `kbf-mac-provision` | 9852 | lib, bin `kbf-mac-provision` | checks and applies a Mac worker's host settings from a profile file | none | Mac |
| `kbf-guest` | 3208 | lib, bin `kbf-guest`, bin `kbf-guest-act` (test) | the agent inside a macOS VM guest; the VM driver that calls it is not built (draft #392) | none | Mac, planned |
| `kbf-vmm` | 283 | bin `kbf-vmm` | the VM helper: a stub that answers `--version`, kept so the signing entitlement allow list has its one binary | none | Mac, planned |
| `kbf-alert` | 1532 | lib | an alert book with hysteresis, an outbox and a webhook notifier; nothing raises alerts | none | planned |
| `kbf-estimator` | 7 | lib | a crate comment and no code (learned action sizes) | none | planned |
| `kbf-bench` | 2816 | lib, bin `kbf-bench` | times blob uploads and reads against a server's CAS | none | operator, optional |
| `kbf-baseline` | 1525 | lib, bin `kbf-baseline` | client-side farm numbers from buck2 event logs, comparable between farms | none | operator, optional |
| `kbf-sim` | 1388 | lib | the deterministic simulation kernel (virtual clock, seeded faults) | kbf-sim-cell; *kbf-raft, kbf-sched, kbf-server* | test |
| `kbf-sim-cell` | 4420 | lib | a whole server, daemons and store simulated by seed | none | test |
| `kbf-testkit` | 2587 | lib, bin `kbf-act` | programs tests run in place of `sh -c` | kbf-guest, kbf-it; *kbf-daemon, kbf-driver-container, kbf-driver-native, kbf-mac-session, kbf-node* | test |
| `kbf-it` | 10299 | lib, bins `kbf-cell`, `kbf-m2-act` | repository lints, the layering test, the end-to-end harnesses (`kbf-cell m1`, `m2`, `pki`) | *kbf-node* | test, CI |
| `kbf-coverage` | 1574 | lib, bin `kbf-coverage` | the per-crate coverage ratchet CI runs | none | CI |
| `xtask` | 6896 | lib, bins `xtask`, `xtask-fake` (test) | `cargo xtask <verb>`: CI steps as Rust (fetch, package, sign, budget) | kbf-it; *kbf-mac-session, kbf-vmm* | CI |

The 9 binaries a farm or its operator runs: `kbf-server`, `kbf-daemon`, `kbf-deploy`,
`kbf-updater`, `kbf-mdm-gate`, `kbf-mac-session`, `kbf-mac-provision`, `kbf-bench`,
`kbf-baseline`. CI packages and attests two of them, `kbf-server` and `kbf-daemon`
([artifacts.md](../artifacts.md)); there are no releases yet.

### The dependency graph

Normal (not dev) edges, each crate under the crates it uses, leaves first. A crate's
dev-dependencies are in the table above, in italics.

```text
leaves      kbf-types  kbf-proto  kbf-caps  kbf-objstore  kbf-raft  kbf-host  kbf-auth
            kbf-mdm-api  kbf-mdm  kbf-testkit  xtask
            kbf-alert  kbf-estimator  kbf-baseline  kbf-coverage  kbf-mac-provision  kbf-vmm

pure cores  kbf-sim         <- types
            kbf-meta        <- types
            kbf-segments    <- types
            kbf-sched       <- caps, types
            kbf-invocation  <- sched, types

server      kbf-store       <- raft
            kbf-tier        <- meta, segments, store
            kbf-log         <- caps, meta, proto, sched, types
            kbf-front       <- auth, caps, invocation, meta, objstore, proto, sched,
                               segments, store, tier, types
            kbf-server      <- auth, caps, front, invocation, log, mdm-api, meta,
                               objstore, proto, raft, sched, store, tier, types

worker      kbf-transfer    <- proto, segments, types
            kbf-outputs     <- proto
            kbf-mac-session <- types
            kbf-daemon      <- caps, proto, transfer, types
            kbf-driver-container <- daemon, proto, types
            kbf-driver-native    <- daemon, mac-session, outputs, proto, types
            kbf-node        <- daemon, driver-native, outputs, transfer, types,
                               driver-container (Linux only)

operator    kbf-deploy      <- host, mdm, types
            kbf-updater     <- host
            kbf-bench       <- daemon, proto, transfer
            kbf-guest       <- testkit (for its test-only `kbf-guest-act`)

tests       kbf-sim-cell    <- caps, daemon, front, log, meta, raft, sched, server,
                               sim, store, types
            kbf-it          <- daemon, deploy, objstore, testkit, transfer, xtask
```

The worker side never depends on the server side, except through dev-dependencies: the
daemon's tests start a real `kbf-front` and `kbf-server`.

### Where the docs and the code disagree today

These are not layout problems, but a reader of the map meets them, so they are listed here
and each gets its own fix:

- ARCHITECTURE.md's crate table has 36 rows; it leaves out `kbf-auth`, `kbf-outputs`,
  `kbf-testkit` and `xtask`.
- `docs/design/failure-classes.md` says `kbf-alert` "holds a one-line crate comment and no
  code"; it has an alert book, an outbox and a notifier, with tests.
- `kbf-log`, `kbf-meta`, `kbf-sim-cell`, `kbf-server` and `log.proto` cite
  `docs/design/ha.md`, which is not on `main`; it is draft #303.
- `kbf-mdm-api` says it is "what `kbf-server` and `kbf-mdm-gate` agree on", but
  `kbf-mdm` (the gate) does not depend on it: the gate serves HTTP/JSON under
  `/v1/macs`, while the server's `mdm` client dials gRPC `kbf.mdmgate.v1`, whose only
  server is a fake in `kbf-server`'s tests. No `kbf-server` flag turns the client on.
- `kbf-host` calls itself "the one way kbf changes a host", but `kbf-mac-provision` and
  `kbf-mac-session` each define their own `Host` trait.
- Output collection is written twice: `kbf-outputs` (used by the native driver) and
  `kbf-driver-container`'s own `outputs.rs` and `remove.rs`, both walking with
  `O_NOFOLLOW` under the same kind of limits.
- Three programs play `podman` in tests: `kbf-fake-podman`, `kbf-podman-stub` and
  `xtask-fake`.

## 2. The proposal

Five rules decide each move:

1. **A pure crate stays a crate.** Purity (no clock, randomness, network or hashed
   collections) is enforced per crate, by the layering test in `kbf-it` and the deny in
   each `lib.rs`. Merging a pure crate into an impure one loses that check, so `kbf-types`,
   `kbf-sched`, `kbf-meta`, `kbf-segments`, `kbf-caps`, `kbf-raft` and `kbf-invocation`
   stay.
2. **A library with one consumer merges into it**, unless it is pure or a separate
   program needs it.
3. **A crate with no code and no consumer is deleted.** Git keeps it.
4. **The crate is named after what it builds.** A crate whose product is the binary
   `kbf-x` is called `kbf-x`.
5. **A name means one thing in the repository.**

### 2.1 Merges and deletions

| Change | Why | Cost |
|---|---|---|
| Delete `kbf-estimator` | 7 lines, a comment and no code; no consumer. scheduler.md keeps the design | the workspace entry, the layering test's `PURE` list, one `coverage-baseline` line, one ARCHITECTURE row |
| `kbf-coverage` becomes `cargo xtask coverage` | CI-only, like every other `xtask` verb; one place for CI logic | the CI coverage step, the `coverage-baseline` header; no imports (no crate depends on it) |
| `kbf-log` into `kbf-server` (as `kbf_server::log`) | its consumers are `kbf-server` and `kbf-sim-cell`, and `kbf-sim-cell` already depends on `kbf-server`; it exists to keep codecs out of the pure crates, which a module of the server does as well. Its name reads as "logging" | 18 lines in 9 files (7 in `kbf-server`, 2 in `kbf-sim-cell`); `kbf-server` grows by about 4000 lines and compiles as one unit |
| `kbf-auth` into `kbf-front` (as `auth`) | its two consumers are `kbf-front` and `kbf-server`, and the server already depends on the front | 43 lines in 22 files (11 in each) |
| `kbf-outputs` into `kbf-daemon` (as `outputs`) | both drivers already depend on `kbf-daemon`; in the library both drivers share, the container driver can then drop its own copy (a separate change with its own tests, not part of the move) | 85 lines in 20 files (18 in `kbf-driver-native`, 2 in `kbf-node`) |

Not merged, and why:

- `kbf-store` and `kbf-tier` do I/O behind traits that the simulation replaces
  (`FaultFs`); they are separate from `kbf-server` so `kbf-sim-cell` can drive them.
- `kbf-host` has two consumers (`kbf-deploy`, `kbf-updater`) that are separate programs.
- `kbf-proto` (504 lines) is generated code that 11 crates share.
- `kbf-mdm-api` waits for [the protocol question](#24-open-questions).
- `kbf-mac-session`, `kbf-mac-provision`, `kbf-updater` and `kbf-mdm` are separate
  programs with separate privileges (three run as root), signed and installed separately.
- `kbf-sim` and `kbf-sim-cell`: the kernel is a dev-dependency of three pure cores;
  the cell depends on the server. One crate would put the server under the pure cores'
  tests.

Result: 40 crates become 35, before the open questions.

### 2.2 Renames

| Today | Proposed | Why | Cost |
|---|---|---|---|
| `kbf-node` (builds `kbf-daemon`) | `kbf-daemon` | rule 4: the binary's crate carries its name | the swap below; `-p kbf-node` in `artifacts.yml` and the SBOM path `crates/kbf-node/kbf-daemon_bin.cdx.json` |
| `kbf-daemon` (the library) | `kbf-worker` | frees the name; the protocol is already `kbf.worker.v1` and the server's listener `--worker-listen` | 164 lines in 73 files, most in the two drivers (30 and 27 files) |
| `kbf-front` | `kbf-reapi` | "front" also names the Envoy client front (`kbf-deploy envoy`, deployment-topology.md); this crate is the REAPI services | 138 lines in 59 files, 48 of them in `kbf-server` |
| `kbf-mdm` | `kbf-mdm-gate` | rule 4: it builds `kbf-mdm-gate` | 12 lines in 3 files (`kbf-deploy`) |
| `kbf-store` | `kbf-raft-store` | beside `kbf-objstore`, the outputs `Store` and the `--store` flag, "store" is ambiguous; this one is the Raft log's disk | 38 lines in 24 files |

The `kbf-node`/`kbf-daemon` swap has to land as one pull request: the library is renamed
first in the same commit series, so no commit has two crates named `kbf-daemon`.

None of these names is on crates.io (`kbf`, `kbf-server`, `kbf-daemon`, `kbf-worker`,
`kbf-reapi`, `kbf-raft`, `kbf-types` and `kbf-sim` all answered 404 there), and nothing is
published, so a rename costs imports only.

### 2.3 Groups

Two ways to make the groups visible:

**A. Directories** (recommended):

```text
crates/
  bin/      programs a farm runs: kbf-server, kbf-daemon, kbf-updater,
            kbf-mdm-gate, kbf-mac-session
  ops/      programs a person runs: kbf-deploy, kbf-mac-provision, kbf-bench,
            kbf-baseline
  lib/      kbf-types, kbf-proto, kbf-caps, kbf-sched, kbf-meta, kbf-invocation,
            kbf-raft, kbf-raft-store, kbf-segments, kbf-objstore, kbf-tier,
            kbf-reapi, kbf-worker, kbf-transfer, kbf-host, kbf-mdm-api, kbf-alert
  drivers/  kbf-driver-container, kbf-driver-native, kbf-guest, kbf-vmm
            (and kbf-driver-vm, draft #392)
  testing/  kbf-sim, kbf-sim-cell, kbf-testkit, kbf-it
xtask/      at the root, where `cargo xtask` projects usually keep it
```

5 + 4 + 17 + 4 + 4 + 1 = 35. A newcomer reads two directories (`bin/`, `ops/`) and
ignores the rest.

The cost is one mechanical pull request, but a wide one. Today the paths `crates/<name>`
appear 125 times in 24 Markdown files, 35 times in workflows, 138 times in Rust sources
(include paths, test fixtures, the budget scan), in 54 `../../` relative paths that gain a
level, and in 35 rows of `tools/interpreter_budget.tsv`. The workspace's
`members = ["crates/*"]` becomes `["crates/*/*", "xtask"]`, and the coverage ratchet
attributes a file to the first directory under `crates/` (`ratchet::by_crate`), so it
must learn the package roots, or it would count all of `lib/` as one crate. Every open
pull request that touches a crate needs a merge of `main` afterwards; seven are open
today, so the move goes last and waits for a quiet day.

**B. A flat `crates/` and names only.** Keep `crates/*`, and make each crate say its
group in its `description` and in a README table. Much cheaper (no paths change), but
GitHub's directory page still lists 35 names in alphabetical order with the Mac helpers
between the scheduler and the server.

Either way, the README gets the table of section 3.3.

### 2.4 Open questions

1. **The MDM gate's protocol.** `kbf-server`'s client speaks gRPC `kbf.mdmgate.v1`
   (`kbf-mdm-api`), the gate serves HTTP/JSON. One has to go. Recommendation: keep the
   gate's HTTP/JSON (it is the one that runs, and an operator can read it with curl), move
   the shared names into `kbf-mdm-gate`'s library behind a `client` feature, and delete
   `kbf-mdm-api` and its proto. That makes 34 crates. The alternative is that the gate
   serves `kbf.mdmgate.v1` and `kbf-mdm-api` stays.
2. **`kbf-alert`, built and unused.** Keep it under `lib/` until the server raises its
   first alert, or delete it until then. Recommendation: keep it (it is tested and three
   design documents plan for it), and say "not wired" in its description.
3. **Publishing.** Nothing is published. Recommendation: keep `publish = false`
   everywhere until the server and daemon have a release; publish no library before
   then. A crate that is later published (the likely first ones are `kbf-raft` and
   `kbf-objstore`, which depend on no other kbf crate) gets a README
   then.
4. **Directories or names** (section 2.3). Recommendation: A, last in the order.
5. **The Mac host abstractions.** `kbf-host`, `kbf-mac-provision`'s `Host` and
   `kbf-mac-session`'s `Host` are three ways to change a machine. Folding
   `kbf-mac-provision` into `kbf-deploy` as a `mac` kit over `kbf-host` would make one;
   it is about 10000 lines and changes behaviour, so it is a design change of its own,
   not part of this layout.

## 3. The outside user's path

### 3.1 A single-node farm today

There is no single-node guide today. `docs/deploy/` describes a converged storage node
with MinIO and systemd units, installed by `kbf-deploy linux`; that is a production
layout. The shortest path that works is the one CI's end-to-end harness runs
(`kbf-cell m2`, crate `kbf-it`): one `kbf-server` with memory storage and one
`kbf-daemon` with the container driver, on one Linux host, buck2 pointed at it.

1. **Get the binaries.** There are no releases yet, and CI's attested builds are workflow
   artifacts kept for at most 90 days, so a user builds from source:
   `cargo build --release --locked -p kbf-server -p kbf-node --bins`.
2. **Make certificates.** The daemon only connects over mutual TLS. The one tool that
   writes a CA, a server certificate for `localhost` and a client certificate is a test
   harness: `cargo run --release -p kbf-it --bin kbf-cell -- pki --dir pki --node-id
   node-1` writes `ca.pem`, `server.pem`, `server.key`, `client.pem`, `client.key`.
3. **Run the server**, REAPI in plain text on loopback, the worker listener in mutual TLS:

   ```text
   kbf-server --listen 127.0.0.1:8980 --worker-listen 127.0.0.1:8981 \
     --worker-tls-cert pki/server.pem --worker-tls-key pki/server.key \
     --worker-client-ca pki/ca.pem
   ```

   `--store` and the metadata log default to memory: a restart forgets the cache.
4. **Run the daemon.** With `--driver fake` it connects and runs nothing (every action
   "succeeds" empty), which proves the TLS setup and no more:

   ```text
   kbf-daemon --driver fake --server https://127.0.0.1:8981 --tls-server-name localhost \
     --ca-cert pki/ca.pem --cert pki/client.pem --key pki/client.key --node-id node-1
   ```

   To run builds, `--driver container` adds `--cas https://127.0.0.1:8981` and
   `--scratch DIR`, and needs a user with rootless Podman, subordinate ids and a
   delegated cgroup: the unit in [linux-build-host.md](../deploy/linux-build-host.md),
   or `systemd-run` with delegation as the m2 harness does. An action names its image by
   digest in the `container-image` platform property.
5. **Point the build tool at it.** buck2: `[buck2_re_client]` with `engine_address`,
   `action_cache_address` and `cas_address` set to `grpc://127.0.0.1:8980`, `tls =
   false`, and an execution platform that sets `container-image`; the sample project
   `crates/kbf-it/m2/buck2` is a complete one. Bazel: `--remote_executor` and
   `--remote_cache` at the same address (`crates/kbf-it/m1/bazel/.bazelrc`).

What is missing for this to be a guide: a release to download, certificates from a
program meant for users, and a page that says the five steps. Section 4 adds the last two.

### 3.2 What a new user never needs to read

- The Mac worker crates and their documents: `kbf-mac-session`, `kbf-mac-provision`,
  `kbf-mdm`, `kbf-mdm-api`, `kbf-guest`, `kbf-vmm`, `kbf-driver-native`;
  `macos-*.md`, `mac-node-provisioning.md`, `mdm-backend.md`, `ios-devices.md`.
- Rolling updates: `kbf-updater`, `fleet-updates.md`, `fleet-updates-security.md`.
- Everything under the testing group, `xtask`, `kbf-coverage`, `docs/spikes`,
  `tools/spike`.
- The internals of storage and consensus (`kbf-raft`, `kbf-store`, `kbf-tier`,
  `metadata-snapshots.md`, `write-tier-scenarios.md`, `simulation.md`,
  `failure-classes.md`), unless they change them.
- `kbf-deploy`'s `mdm`, `envoy` and `minio` kits, until they run more than one node.

### 3.3 README and ARCHITECTURE.md changes

**README.md** gains, after the opening paragraph:

- *Quick start*: the five steps above, as a link to a new `docs/quickstart.md`.
- *What is in this repository*: one table, by group, one line per crate, with the 2
  programs a user runs first (`kbf-server`, `kbf-daemon`), then the optional operator
  tools, then "Mac workers", then "everything else is libraries and tests; see
  ARCHITECTURE.md". The README stops saying "two programs" only, since there are nine a
  farm or operator may run, while keeping the two at the top.

**ARCHITECTURE.md**:

- "The two programs" becomes "The programs": the two, and one line each for the seven
  others and who runs them.
- The crate table lists every crate (it misses four today), sorted by group, with a
  group column next to "Pure", and the `kbf-node` paragraph about the Cargo cycle goes
  away once the binary's crate is `kbf-daemon`.
- A test in `kbf-it` keeps the table honest: every workspace member has a row and every
  row names a member, as `deploy_units` already does for the units in `docs/deploy/`.

## 4. The step order

Each step is one small pull request, green on its own, in this order. Steps 1 to 3 need no
decision; the rest wait for the answers to section 2.4.

1. **Descriptions.** Every `Cargo.toml` gets a `description`: one line, the "What it is"
   column above, starting with its group. A `kbf-it` test requires one in every
   member; the pull request shows it red with one description removed.
2. **Delete `kbf-estimator`.** Workspace entry, layering list, coverage line, docs.
3. **The doc fixes of section 1** (failure-classes.md on `kbf-alert`, the ARCHITECTURE
   table's four missing rows).
4. **`kbf-coverage` into `xtask coverage`.**
5. **`kbf-log` into `kbf-server`**, then **`kbf-auth` into `kbf-front`**, then
   **`kbf-outputs` into `kbf-daemon`**: three pull requests, each a move with no
   behaviour change, so the existing tests are the check (each crate's tests move with
   its code, and the coverage baseline line moves to the crate that absorbs it).
6. **Renames**: `kbf-mdm` to `kbf-mdm-gate`, `kbf-store` to `kbf-raft-store`, `kbf-front`
   to `kbf-reapi`, then the `kbf-node`/`kbf-daemon` swap (which also touches
   `artifacts.yml` and `docs/artifacts.md`). One pull request each.
7. **Certificates for users**: `kbf-cell pki` becomes a `kbf-deploy` verb, with the
   harness calling the same code, and `docs/quickstart.md` with the five steps of
   section 3.1. The m2 harness keeps proving the steps work.
8. **The MDM gate protocol**, as decided (section 2.4, question 1).
9. **README and ARCHITECTURE** (section 3.3), with the crate-table test.
10. **Directories** (section 2.3, option A), if chosen: one mechanical pull request,
    with the coverage ratchet taught package roots in the same change, at a time when
    few pull requests are open.
11. **READMEs**: one short README for each program crate (what it is, how to run it, its
    flags by `--help`, the document that explains it), and for each library once it is
    published (section 2.4, question 3). `kbf-bench` and `kbf-mac-provision` already
    have one.

Follow-ups that are not layout, opened as their own issues: one output collector for both
drivers, one fake `podman` for tests, and the three host abstractions.
