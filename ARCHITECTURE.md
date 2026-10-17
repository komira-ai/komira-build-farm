# kbf architecture

This document describes how kbf is put together: what each part does, how a request
moves through the farm, where state lives, and how the code is tested. It describes
the code on `main`. Parts that are designed but not built yet are marked
**planned**; nothing marked planned exists in the code today.

The design documents under [docs/design](docs/design) go deeper:

| Document | Covers |
|---|---|
| [scheduler.md](docs/design/scheduler.md) | operations, leases, fencing, QoS, placement, accounting |
| [simulation.md](docs/design/simulation.md) | the scheduler's simulations: what exists, the invariants every step checks, the scenario families to build and the mutants each must catch |
| [storage.md](docs/design/storage.md) | the CAS and action cache, segments, metadata, retention, the object store interface |
| [write-path.md](docs/design/write-path.md) | uploads grouped into shared segments (one PUT and one metadata commit per segment), segment upload from the daemon (an action's outputs as whole segments, one metadata commit) and segment-range fills, a local NVMe/SSD write tier flushed to the object store, what an acknowledgement promises at each step, crash and lost-disk recovery, rollout and tests (grouping built; the rest **planned**) |
| [write-tier-scenarios.md](docs/design/write-tier-scenarios.md) | every failure the local write tier must survive (crash points, store outages of minutes and hours, slow and failing stores, lost commits, disk full, bad sectors, torn tails, eviction and read races, node loss, leader change, clock jumps, the orphan sweep), the invariant each checks and the outcome expected, which run today against the direct path and which wait for the tier, and the simulation seams the tier plugs into (**planned**; the direct-path runs exist) |
| [metadata-snapshots.md](docs/design/metadata-snapshots.md) | the snapshot encoding of the metadata state, compaction of the single-voter log, starting from a snapshot, version refusal, size and pause at today's scale and 10x, and how the HA design inherits it (**planned**) |
| [worker-protocol.md](docs/design/worker-protocol.md) | `kbf.worker.v1`: messages and the rules both sides keep |
| [daemon.md](docs/design/daemon.md) | `kbf-daemon`, the container driver, output collection, limits |
| [worker-cache.md](docs/design/worker-cache.md) | the daemon's blob traffic: one shared transfer engine (one connection, one in-flight limit, batching, FindMissing before upload), a per-node NVMe input cache linked or cloned into each lease, per-driver safety, eviction, metrics, rollout and tests (the engine without batching, and the container driver's cache, are built) |
| [capabilities.md](docs/design/capabilities.md) | node reports, ISA levels, matching an action to a node |
| [mac-node-provisioning.md](docs/design/mac-node-provisioning.md) | a Mac as a worker: baseline, provisioning, the signed daemon artifact, headless settings, updates, join and leave (**planned**) |
| [macos-end-state.md](docs/design/macos-end-state.md) | the macOS native and whole-machine drivers' end state: a root daemon, a reused pool of unprivileged lease users with a clean and post-clean check between leases, the macOS worker cache by hard link, one auto-login GUI user for GUI and Screen Recording leases, stable farm signing, the probes and build order (**planned**) |
| [macos-vms.md](docs/design/macos-vms.md) | what runs on bare metal on a Mac and what in a macOS VM, VM sizing and scheduling, the VM driver, GPU tests (**planned**) |
| [macos-vm-guests.md](docs/design/macos-vm-guests.md) | a VM guest's first-boot setup, capture inside the guest, guest networking, image identity (recipe and content digests), the VM helper's uid and signing, the probes a real Mac must run (**planned**) |
| [fleet-updates.md](docs/design/fleet-updates.md), [fleet-updates-security.md](docs/design/fleet-updates-security.md) | keeping node software current: rolling updates, MDM on Macs, Linux host updates, bare-metal GPU and app-install isolation, the Fleet UI; its security model: threat model, root helpers, signing keys, the MDM gate, enrollment (**planned**) |
| [deployment-topology.md](docs/design/deployment-topology.md) | where servers run, the Raft log on local disk, the client front that routes reads and uploads to any server and the rest to the leader, daemons dialling the servers directly, the converged topology (servers, object store and daemons on the same hosts), what is built and what is planned, the probes the front must pass (**planned**) |
| [mdm-backend.md](docs/design/mdm-backend.md) | MDM as a pluggable backend behind `kbf-mdm-gate`: the three operations the server uses, erase only by an operator's hardware-key-signed request, macOS 27 update progress, network reachability, moving the MDM, kbf's own configuration management (**planned**) |
| [invocations.md](docs/design/invocations.md) | a build's id (the client's `tool_invocation_id`), the per-invocation and per-action record, where it is kept and for how long, the read API an agent diagnoses a build with (a failures view with log tails, artifacts, metrics, a one-command `kbf build show` client, read tokens; the agent-facing part is [invocations-api.md](docs/design/invocations-api.md)), the configurable build-page URL and how a buck2 or Bazel user gets the link (**planned**) |

Decision records live in [docs/adr](docs/adr).

## What kbf is

kbf is a remote build and test farm for [Bazel](https://bazel.build) and
[Buck2](https://buck2.build). Build tools talk to it with the
[Remote Execution API v2](https://github.com/bazelbuild/remote-apis) (REAPI): they
upload inputs to a content-addressable store (CAS), look up results in an action
cache (AC), and ask the farm to execute actions that miss. kbf runs those actions on
worker machines and stores the results so the next build, on any machine, can reuse
them.

There is no client-side kbf component. Bazel and Buck2 speak REAPI to `kbf-server`
directly.

## The two programs

kbf ships two binaries.

**`kbf-server`** (crate `kbf-server`) serves REAPI to build clients and
`kbf.worker.v1` to daemons, on two separate listeners. It holds the cache, the
scheduler and the record of which daemons are connected. Today one process runs every
role (`--role=all`). The flags are:

- `--listen` (REAPI, default `127.0.0.1:8980`) and `--worker-listen` (daemons, default
  `127.0.0.1:8981`);
- `--reapi-tls-cert` and `--reapi-tls-key` to serve the REAPI listener over TLS (both,
  or neither for plain text);
- `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` to serve the worker
  listener over mutual TLS (all three, or none for plain text), and
  `--worker-deny-list` for the certificates and nodes it refuses. TLS and mutual TLS
  are optional: a listener without them serves plain text on any address, with a
  warning on the start line when that address is not loopback (see
  [Security model](#security-model));
- `--cold-store`, the durable object store, a URI: `s3://bucket/prefix?endpoint=...`
  (query keys `endpoint`, `region`, `conditional-put`, `min-put-rate`; the S3 key pair
  comes from the standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` variables,
  never from the command line) or `file:///dir`, a directory on the local disk (under
  the fixed prefix `kbf/<log id>/`), which refuses writes that would leave its file
  system with less than 5% of it (at least 1 GiB) free and evicts the least recently
  used objects to stay above that; `--hot-store`, optionally, a `file://`
  directory on a fast disk in front of it (the write tier, sized from its filesystem's
  free space, at most `--hot-store-max-bytes`); and the metadata log's directory,
  `<hot store>/meta` or `--meta-dir`. There is no `memory:` store: a `file://` path on a
  tmpfs is one (storage glossary: [storage-node.md](docs/deploy/storage-node.md#where-the-bytes-live-storage-glossary));
- `--heartbeat-interval-ms` (default 5000), the interval daemons are asked to
  heartbeat at;
- `--finished-retention-secs` (default 60), how long a finished operation is kept
  for WaitExecution before its name is `NOT_FOUND`;
- `--shutdown-timeout-secs` (default 10), how long a stop (SIGINT or SIGTERM) waits
  for REAPI clients to disconnect after their open Execute and WaitExecution streams
  that are not done are ended `UNAVAILABLE` (a finished operation a stream is waiting
  on is still sent first);
- `--invocation-memory` (bytes, default 268435456, 256 MiB), the memory the invocation
  records take together ([invocations.md](docs/design/invocations.md) section 3.5):
  each build's calls, leases and outcomes, kept in memory only and lost on restart;
- `--api-listen`, off unless given: the operator API, HTTP/JSON under `/v1`
  ([docs/api.md](docs/api.md)). Reads are open, so bind it where only operators
  reach it; writes need the token in `--api-token-file` (owner-only file), come from
  loopback, and carry no `Origin` header. Run `kbf-server` as a user other than
  `kbf-daemon`'s and its lease users', or builds on the same host can read the token.

**`kbf-daemon`** (crate `kbf-daemon`) runs on each worker machine. It opens one
outbound mutual-TLS stream to a server, reports what the machine is, heartbeats, and
runs the leases the server starts. It never listens on a port. Execution goes through
a `Runtime` trait, and the binary's `--driver` flag picks the implementation:
`container` runs each action in a fresh rootless `crun` container (crate
`kbf-driver-container`, Linux only; images come from `kbf-image`), `native` runs it as plain processes, for Macs
(crate `kbf-driver-native`), and `fake` runs nothing, for bring-up. The binary is built
from crate `kbf-node`, not from the `kbf-daemon` library crate, because the drivers
depend on that library and a binary in it that named them would be a cycle Cargo
refuses.

**`kbf-all-in-one`** (crate `kbf-all-in-one`, Linux only, the container driver only) runs
both in one process, for a one-node farm: the server half as `kbf-server` starts, then
the daemon half as `kbf-daemon` starts, each on an async runtime of its own, taking every
flag of both binaries. The daemon half's control plane is a remote daemon's (mutual TLS
to the server half's worker listener on loopback); its blobs move through calls into the
server half's cache and `Segments` handlers (`LocalDataPlane`), with no listener between.
Not yet production-ready: a restart loses the scheduler's state, which is in memory, and
the deploy shape, the unit and the ownership step are not written
([README](crates/kbf-all-in-one/README.md)). The same crate holds `kbf
storage-and-daemon`: the storage node (`kbf_server::storage_half`, `kbf storage`'s start
with the storage it serves kept) and the same daemon half over the same `LocalDataPlane`,
which knows the store only as `kbf_storage::Storage`.

## Crates

kbf is one Cargo workspace. Some crates are **pure**: they hold decision logic, do
no I/O, read no clock and draw no random numbers. Time and entropy are inputs. This is
what lets a simulation seed replay a run exactly (see [Testing](#testing)).

| Crate | Pure | What it holds |
|---|---|---|
| `kbf-types` | yes | digests, images by manifest digest (`ImageRef`), platforms, QoS levels, lease ids, farm time, the `StateMachine` trait and `Effect` enum |
| `kbf-sched` | yes | the scheduler state machine and the fence constants |
| `kbf-meta` | yes | the metadata state machine: CAS index, action cache, closure check, retention |
| `kbf-segments` | yes | the segment format and FastCDC chunking |
| `kbf-caps` | yes | CPU capability parsing, ISA levels, request matching |
| `kbf-raft` | yes | a sans-IO Raft core ([ADR 0001](docs/adr/0001-consensus.md)) and its host loop over `Storage`, `Transport` and `Machine` traits, with an in-memory storage |
| `kbf-invocation` | yes | the invocation record ([invocations.md](docs/design/invocations.md) section 3): each build's header, counters and action rows, built only from events (calls, lease grants, starts, requeues, finishes), its `kbf.invocation.v1` encoding, and the memory bound of section 3.5 (minted, then idle records evicted; then the largest open one closed to new rows, and whatever else does not fit left out) |
| `kbf-map` | yes | the storage map (generation, members with address, zone, weight and state, rule, replication factor, minimum protocol, deny entries) and weighted rendezvous hashing over it: write owners, read order, and read order across two generations ([architecture-v2](docs/design/architecture-v2.md) sections 6 and 8); depends on `kbf-types` and a fixed-key SipHash only, so the frontend, storage, the daemon and the transfer engine can all use it |
| `kbf-mapsync` | no | the storage map in motion: the `MapSource` seam a frontend implements (the map it holds and a generation that ticks when it changes), and the `MapHolder` a daemon or a storage node keeps the newest pushed map in, never going back ([architecture-v2](docs/design/architecture-v2.md) section 6); depends on `kbf-map` and tokio's watch channel only |
| `kbf-estimator` | yes | placeholder for learned action sizes (**planned**) |
| `kbf-objstore` | no | the `ObjectStore` trait, an in-memory store, a local-directory store (`FsStore`, `kbf-server --cold-store=file://...`), an S3 store, the conformance suite |
| `kbf-tier` | no | the local write tier's disk layer ([write-path.md](docs/design/write-path.md) section 6): block files in the hot store directory (`kbf-server --hot-store`) written in reserved ranges and synced (file and name) before a range may be committed, seals at the segment limit or after the seal-after interval, the hot store's budget, and the restart: the `TIER` check, staged blocks cut to their committed length, and the orphan sweep, against the committed state through `StagedIndex`; once flushed, blocks become `.seg` copies evicted least recently read first, only in what the budget leaves over. `kbf-front` (`Cache::attach_tier`, `Cache::flush_tier`) stages writes on it, reads staged blobs from it, and runs the flusher; `kbf-server` attaches it with `--hot-store` over either metadata log (under the Raft metadata log it first raises the log to machine version 2, once; a server without a tier never raises it) |
| `kbf-state` | no | the state store trait the scheduler writes every change through ([architecture-v2](docs/design/architecture-v2.md) section 4.1): compare-and-set with versions, multi-key transactions, counters, time-to-live keys on the store's own clock, a change feed a poller resumes without skipping a row, durable acknowledgements, group commit and a bounded range scan under a prefix, between two bounds, forward or in reverse, that pages with a resume token; an in-memory fake with an injectable clock; the SQLite backend (`sqlite`: one writer thread, WAL with a sync per group commit, a 5 ms commit window, `flock` on `<file>.lock`, a network filesystem refused (on Linux only), the epoch bumped on every open, the online `backup` (`VACUUM INTO` while the store writes, into a single file) and the check of a backup, the offline `restore` (also under a `StoreLock` the caller holds, the store's own lock), and the commit-latency figures); and, with the `postgres` feature, the Postgres backend (`SERIALIZABLE` batches, revisions from one locked head row, the database's `clock_timestamp()` for expiry, a `SKIP LOCKED` claim path; it refuses a server whose `synchronous_commit` is not on), run against a real server by the `state-postgres` CI job; and the conformance suite C1 to C8 every backend must pass |
| `kbf-history` | no | build history in the object store ([ui-data.md](docs/design/ui-data.md) section 5), as a library not yet wired into the server: the one container format (a versioned header, zstd blocks of length-prefixed protobuf records, a footer with the block index and the family summary, a trailer with the footer's CRC-32C), object keys led by retention class (`r7`, `r30`, `r45`, `r400`) and the `ui/` index row keys; the run, hit and outcome records, tails, command renderings and failed-run sections, each with its `instance`; time-ordered run ids minted from memory (stream blocks from the `ui/streams` counter, a fallback range that never waits); the writer core (chunks cut by size, age, day, stream and close; object written first and index rows second through `kbf-state`; bounded queues and a bounded set of open invocations that drop and count; rows dropped past the bound restored from the object's footer with no LIST); the reader (a run id to its record, an invocation to its header and newest chunk summary, its records a bounded page of chunks at a time, all-must-allow across instances); the index seam (`HistoryIndex`: rows under an `IndexRoot`, every blocking state-store call through an `Offload` the server supplies); and `Scoped`, an object store that refuses every key outside `history/` |
| `kbf-front` | no | the REAPI services over a `Cache` and a `Dispatch`, and the invocation `Recorder` the services and the farm send events to (a channel bounded in events and bytes that drops, never waits) |
| `kbf-storage` | no | the storage boundary ([architecture-v2.md](docs/design/architecture-v2.md) sections 3 and 13): the `Storage` trait (FindMissing, whole-blob read and write, a streamed write, the `Segments` service, the action cache) and its `StorageError`. `kbf-front`'s `CacheStorage` puts the cache behind it and `kbf-server`'s `LocalStore` calls through it; the cache's code is to move here. Depends on no frontend crate: not `kbf-sched`, not `kbf-auth` (`crates/kbf-it/tests/layering.rs`) |
| `kbf-peer` | no | the role a peer's certificate names ([architecture-v2.md](docs/design/architecture-v2.md) section 9): the one reader of its `urn:kbf:role:<name>` URI subjectAltName (`role_of`), which the storage service's per-endpoint table and the worker listener's per-method table (`kbf-server`'s `worker_roles`) ask; no gRPC, so a tool that checks a certificate can use it too |
| `kbf-server` | no | the server: wires front, scheduler and storage together (`start` returns a `ServerHandle`; the `kbf-server` binary is a thin main over it, so a combined binary can run the server in its own process, and `ServerHandle::local` is the server's cache as calls in that process, `LocalStore`; the stores its flags name, `--cold-store`, `--hot-store` and `--meta-dir`, are parsed and resolved in `stores`); its side of the MDM gate (`mdm`) |
| `kbf-proto` | no | generated code for REAPI, `kbf.worker.v1`, `kbf.storage.v1`, `kbf.log.v1` and `kbf.map.v1` (the storage map's messages, with their conversions to and from `kbf-map` types) |
| `kbf-log` | no | the replicated log's entry encoding: `kbf-meta` commands and the control commands (batched farm inputs, a result with its action-cache entry, takeover, checkpoint, machine version) to and from `kbf.log.v1`, under the log's format version |
| `kbf-mdm-api` | no | what `kbf-server` and `kbf-mdm-gate` share: generated `kbf.mdmgate.v1` (status, enforce, withdraw, profile; no erase), the names both check, Apple's catalogue parser and its at-most-daily reader |
| `kbf-daemon` | no | the daemon: session loop, lease manager, input and output trees, the input cache |
| `kbf-transfer` | no | the shared blob transfer engine: the `Cas` trait, and `Transfers`, one per daemon process: one HTTP/2 connection to the worker listener with explicit windows, one in-flight limit and byte budget for every lease, one fetch per digest however many leases ask, FindMissing before every upload, resumed reads after a broken connection, and each lease's `TransferStats` ([worker-cache.md](docs/design/worker-cache.md) section 4) |
| `kbf-driver-container` | no | the rootless `crun` execution driver |
| `kbf-image` | no | the node's image source: a digest-verified registry client (hyper over rustls) and the layer unpacker (whiteouts, ownership, in-root path resolution) |
| `kbf-driver-native` | no | the native execution driver: plain processes, for Macs |
| `kbf-node` | no | the worker daemon as a library (flags, the driver `--driver` names, the data directory `--data-dir` (`leases/`, the input cache and the input trees, sized from the filesystem), `start` returning a `NodeHandle`, over the transfer engine; `start_with`, over a data plane the caller makes) and the `kbf-daemon` binary, a thin main over it. Only a combined binary may link both this and `kbf-server` (`kbf-it` `layering`) |
| `kbf-all-in-one` | no | the server and a daemon in one process (the `kbf-all-in-one` binary, above): one command line of both binaries' flags, and `LocalDataPlane`, the daemon's data plane as calls into the server's `LocalStore` on the server half's runtime; and `kbf storage-and-daemon` (`storage_and_daemon`), the storage node and a daemon in one process over the same plane ([README](crates/kbf-all-in-one/README.md)) |
| `kbf-cas-conformance` | no | the checks every implementation of the daemon's data plane (`kbf_transfer::Cas`) must pass, generic over the implementation (`tests!`); run against the gRPC implementation `kbf_transfer::Transfers`, and in `kbf-all-in-one` against `LocalDataPlane` over a server with a write tier and one without |
| `kbf-threads` | no | thread and disk sizing for both binaries (disk: a filesystem's size and free space, the floor it keeps free, and the shares a cache, a tree store or a hot store may take: `disk`); thread sizing: on the daemon `--reserve-cpus` (and `--reserve-share`) size the pools and leave the leases the other CPUs, limited to the physical cores (`Topology`); the server has no reserve and uses every CPU it may (one async worker and one hashing thread per logical CPU, tokio's blocking cap); `--async-threads`, `--blocking-threads` and `--cpu-threads` override on both; and the tokio runtime built with them |
| `kbf-tls` | no | TLS for a server listener whose certificate, key and client CA are files that change while it runs: `TlsSource` re-reads them on an interval (`--tls-reload-interval-secs`) and swaps in a new `rustls` configuration only if the new files make a working one (readable, PEM, key matches certificate, client CA has a usable certificate, leaf not expired), otherwise it keeps the current one and counts the refusal (`kbf_tls_reloads_total`); `Incoming` accepts the connections a tonic server is given, handshaking each under the configuration current at its accept, so a renewal never ends a connection in flight; `peer_certs` and `remote_addr` read a request's peer. `kbf-server` uses it for the REAPI and worker listeners and the storage node's listener (`kbf-storage` reads a caller's certificate through it, `service/admission.rs`) |
| `kbf-mdm` | no | `kbf-mdm-gate`, the only holder of the Mac MDM's API key: its verbs and caps over mutual TLS, operator-signed erase requests, the `MdmBackend` trait and its NanoHUB client ([mdm-backend.md](docs/design/mdm-backend.md)). With the `testkit` feature, security-key signatures made in software, for other crates' tests |
| `kbf-updater` | no | the root helper that verifies and installs signed software sets on a node ([fleet-updates-security.md](docs/design/fleet-updates-security.md) S3, S4.1); its apt-snapshot applier changes the node only through `kbf-host`; Linux only for now |
| `kbf-mac-session` | no | the Mac's root helper that gives every lease its own throwaway user, admits an administrator only with the MDM gate's signed grant, switches the GUI session to a lease user (auto-login, then a userspace restart) and resets it at boot ([fleet-updates-security.md](docs/design/fleet-updates-security.md) S4.2, S4.3, S5.2); serves on macOS only |
| `kbf-mac-provision` | no | checks and applies a Mac node's host settings and worker profile from a profile file ([README](crates/kbf-mac-provision/README.md) lists every item): its profile parser, the item checks over a `Host` trait (a fake Mac in the tests), and the verbs `check`, `apply`, `print` and `restart`, and `revert` of the power and update settings `apply` records before its first change; `tests/as_root.rs` runs the binary as root on a hosted macOS runner ([mac-node-provisioning.md](docs/design/mac-node-provisioning.md) sections 2, 3, 5) |
| `kbf-guest` | no | the agent inside a macOS VM guest ([macos-vms.md](docs/design/macos-vms.md) section 6): a versioned protocol over a Unix socket, one command per boot behind a per-boot token, its process group killed on exit, `Kill` or timeout; the virtio socket and the VM driver that calls it are **planned** |
| `kbf-vmm` | no | the VM helper for macOS VM leases ([macos-vm-guests.md](docs/design/macos-vm-guests.md)): a **stub** that answers `--version` only, the one binary the signing scripts' entitlement allow list gives an entitlement (`com.apple.security.virtualization`); the VM code is **planned** |
| `kbf-host` | no | the one way kbf changes a host, shared by `kbf-deploy` and `kbf-updater`: a plan of steps (write a file, make a directory, remove, run a tool by absolute path with an empty environment, run one and write its filtered output to a file, wait for an HTTP URL), made from queries that change nothing (a query or tool may run as another user, with that user's groups) and printed as `DRY-RUN:` lines by default and carried out only in apply mode; a file or directory step is planned only when the host differs, so a re-run after an apply plans nothing; a root plan applies only as root, from a binary root alone can replace, and a user plan never as root. With the `testkit` feature, a fake host that records each step and query, for other crates' tests |
| `kbf-deploy` | no | the deploy tool; a kit that changes a host does it through `kbf-host`. It reads closed-key `KEY=VALUE` config files; its unit reader (`unit`) is the one the docs check uses, `template` is strict `@KEY@` substitution, and `https` is a small HTTPS client (rustls with ring, the host's trusted roots). Five kits. `minio`: `buckets` makes buckets, a policy for them and a service account key file through `mc`, with the admin key in `mc`'s environment, never on an argv, and `nodeport` adds a NodePort Service for a MinIO's S3 port through `kubectl`. `linux`: `render-units` renders a converged storage node's systemd units from compiled-in templates and `check-units` checks them against the host's priorities; as root, `install-units` checks the host, installs the binaries, units and directories and starts the services (the daemon fetches its pinned images itself), and `swap` makes the swapfile ([storage-node.md](docs/deploy/storage-node.md#installing-on-the-host)). `mdm`: `fetch` downloads the pinned upstream archives of an Apple MDM (NanoHUB, the SCEP server, NanoDEP, Caddy, lego, mdmctl) as the operator's user and checks each sha256; as root, `install` checks the host and the archives, unpacks the binaries, makes the secrets and the SCEP CA, renders and installs the units and the Caddyfile from compiled-in templates and starts the services; `tls` installs lego's certificate job, its renewal timer and the proxy's reload on renewal, gets the first certificate over DNS-01 and starts the proxy; `push-cert` checks an APNs push certificate and its key, gives them to NanoHUB on loopback and writes the enrollment profile (a property list); `dep` links NanoDEP to Apple Business Manager and defines an ADE profile, refused unless it is supervised, mandatory and not removable; the API calls are printed as steps are and made only with `--apply`; `verify` checks, read only, the services, the listeners, the routes through the HTTPS proxy, the TLS certificate, DNS and the modes of the state and secret files; and, on the operator's own machine and as the operator, `erase-request` writes an erase request for one Mac in the MDM gate's `kbf-erase-v1` text with `kbf-mdm`'s own writer and parser, signs it with the operator's security key through `ssh-keygen -Y sign` (the request on its standard input), and checks the written pair with the gate's own signature check; `envoy`: `render` writes a client front's Kubernetes objects (a Tailscale operator Ingress, an Envoy Deployment, its ConfigMap, Service and NetworkPolicy) and the Envoy config that routes REAPI calls by gRPC method to the kbf-servers over TLS, as JSON whose bytes depend only on the settings: the pod template carries the config's sha256, so a byte change restarts Envoy; `check` checks a render (`manifests.json`, as applied) for what the REAPI path through the front depends on: every client call routed with no bound on a stream's life and an idle timeout over the longest silent stream, TLS to the mounted CA naming each server, health checks on names kbf-server answers, and the Ingress, Service and NetworkPolicy reaching Envoy, one `FAIL <check>: <why>` line per failure; `apply` checks the render, the cluster's prerequisites (the Tailscale IngressClass, the operator's Secret, the namespace, the CA Secret) and a server-side dry run, and only with `--apply` applies it server-side through `kubectl` and waits for Envoy and the Ingress's address; `pilot`: as the operator's own user, never root, `server-install` checks the host (the bind address on the configured interface, free ports, a server certificate that chains to the CA for the configured name with its own 0600 key, a 0600 S3 key file outside every git work tree, a kbf-server binary for this CPU whose `--help` lists every flag its unit passes) and runs kbf-server as a capped systemd user service, waiting for its start line after a restart, and `buckconfig` writes the buck2 overlay that points one linked worktree at it, never replacing or removing a `.buckconfig.local` it did not write |
| `kbf-sim` | no | the deterministic simulation kernel |
| `kbf-it` | no | integration tests, repository lints, the end-to-end harness |
| `kbf-coverage` | no | the coverage ratchet CI runs |
| `kbf-baseline` | no | client-side farm numbers from buck2's own event logs (`--event-log FILE.json-lines.gz`, or `buck2 log show` output): actions by where they ran (remote, remote cache hit, local, local cache, buck2 itself), the build's wall time, nearest-rank p50/p90/p99 and maximum of each action's time and of the farm's reported queue and execution times, the cache hit share and the failed actions, per build and over a set, as a table and as JSON. It reads only the client's log, so the same build against two farms compares line by line |
| `kbf-bench` | no | times blob uploads to a server's CAS, and reads back, as a daemon stores an action's outputs: a tree generated to a shape from a seed (or read from a directory), sent over the daemon's own CAS client and TLS loader by ByteStream or batch calls, a set number in flight, with FindMissingBlobs optional; reports wall time, blobs/s, MB/s and nearest-rank p50/p90/p99 and maximum per kind of request, as a table and as JSON ([README](crates/kbf-bench/README.md)) |
| `kbf-alert` | no | alerts with their exact fix; the alert book (raise and resolve with hysteresis) and the outbox, both pure modules; the webhook notifier, which keeps the outbox in a file. Nothing raises alerts yet |
| `kbf-store` | no | a Raft server's durable state on the local disk, as a `kbf_raft::Storage`: a segmented append-only log of CRC-32C records, the hard state (term, vote) and the newest snapshot, whose segments it covers are removed only once it is durable; a torn tail cut at open, any other damage refused; the first write or fsync error stops the store; all I/O through an `Fs` trait, with a fault-injecting in-memory `FaultFs` for crash tests |
| `kbf-sim-cell` | no | the whole-cell simulation: one server assembled from `RaftMetaLog`, the scheduler and the `FarmMachine` on a fault-injecting disk, with fake daemons, clients and an object store, checked against properties 1 to 7 of the high-availability design and the write path's properties 8 and 9 after every step, with object-store faults (hang, slow, failures, lost answers), crashes at named points of the write path, the local write tier (`kbf-tier` on a second fault-injecting disk, staged writes, the flusher, reads, crashes at named tier points, a full disk, a bad sector, eviction) checked against the tier's invariants (properties 10 to 13), and a seed sweep that reports every failing seed ([simulation.md](docs/design/simulation.md#8-the-cell-kbf-sim-cell), [write-tier-scenarios.md](docs/design/write-tier-scenarios.md)) |
| `kbf-ui` | no | the server-rendered pages ([ui-data.md](docs/design/ui-data.md) 6.1 and 8): pages made only from the read API's JSON documents through a `Source` the server gives them, each declaring the routes it reads (its reader refuses any other) and listing the URLs it read; HTML escaped by construction (`html::Html`, no public raw write); every answer with the `Content-Security-Policy` of section 8 and `nosniff`; one embedded token-based stylesheet, no script. Builds (an id lookup), Build and Action; not mounted on the server yet |

A test in `kbf-it` reads the dependency graph and fails if a pure crate depends,
directly or not, on an async runtime, a network crate or a random source, or if its
`lib.rs` stops denying the items listed in [clippy.toml](clippy.toml) (clocks, sleep,
sockets, hashed collections).

## Request paths

### Cache traffic

`Capabilities`, `ContentAddressableStorage`, `ByteStream` and `ActionCache` are served
from one `Cache` (crate `kbf-front`). The cache keeps its index in a metadata state
machine and its bytes in an object store, packed into segments. The promises it keeps:

- **FindMissingBlobs never omits a digest.** A digest that does not parse fails the
  whole call. A blob whose stored copy cannot be read is reported missing, so the
  client uploads it again.
- **Present means durable.** An upload is acknowledged, and a blob reported present,
  only once its bytes are in the object store and its location is committed to the
  index.
- **Every byte is verified,** against its digest, on upload and on read.
- **A hit never points at missing files.** `GetActionResult` answers through the
  closure check: the result and every blob it names must be present and readable,
  or the lookup is a miss.
- **Clients never write the action cache.** `UpdateActionResult` is always
  `PERMISSION_DENIED`; results reach the cache only from a daemon that ran the action.

See [storage.md](docs/design/storage.md) and, for the planned write path,
[write-path.md](docs/design/write-path.md).

### Executing an action

```
client                      kbf-server                               kbf-daemon
  | Execute                    |                                         |
  |--------------------------->| 1. AC lookup (closure check): hit -> done
  |                            | 2. Action, Command, input tree in CAS?  |
  |                            |    no -> FAILED_PRECONDITION (MISSING)  |
  |                            | 3. submit: join a running twin, or queue|
  |<-- QUEUED -----------------| 4. place on a worker, propose the lease |
  |                            |--- LeaseOffer ------------------------->| (nothing runs)
  |                            | 5. lease committed                      |
  |<-- EXECUTING --------------|--- Start ------------------------------>| run the lease
  |                            |                                         | fetch inputs (ByteStream)
  |                            |                                         | run, upload outputs
  |                            |<-- Result ------------------------------|
  |                            | 6. holder check, outputs all stored?    |
  |                            | 7. accept, write the AC entry           |
  |                            |--- ResultAck -------------------------->|
  |<-- done (ExecuteResponse) -|                                         |
```

1. **Action cache.** Unless the client set `skip_cache_lookup`, a hit that passes the
   closure check answers at once with `cached_result` set. Nothing runs.
2. **Inputs.** The `Action`, its `Command` and every blob of its input tree must be in
   the CAS; each one that is not becomes a `MISSING` violation in a single
   `FAILED_PRECONDITION`, as REAPI asks.
3. **Submit.** The front hands the request to the scheduler through the `Dispatch`
   trait. If an identical action (same instance name and action digest) is already
   in flight and both may be joined, the caller is attached to it (in-flight dedup).
4. **Place.** The scheduler grants a lease on a worker with room and asks for the grant
   to be committed. The daemon is told with a `LeaseOffer`, on which it runs nothing.
5. **Start.** Only once the grant is committed does the daemon get `Start`.
6. **Result.** The server accepts a result only from the daemon holding the
   operation's current lease, and only if every output it names is already stored.
7. **Answer.** An accepted successful result (exit code 0, not `do_not_cache`) is
   written to the action cache *before* the callers are answered.

`WaitExecution` streams the same stages for an operation name that Execute returned.
A finished operation is kept for `--finished-retention-secs` (60 by default), in which
WaitExecution streams its done operation, so a client whose Execute stream broke still
gets a result the action cache does not keep; then it is forgotten, and the next
Execute for it is answered from the action cache. An operation name is `operations/{term}-{n}`: the server process's term
(the one its leases carry) and a count that starts at 0 in each process. A name from
an earlier process, or any other name, is `NOT_FOUND`, so a client still holding one
after a restart is never attached to another action's operation.

See [scheduler.md](docs/design/scheduler.md) and
[worker-protocol.md](docs/design/worker-protocol.md).

## Where state lives

| State | Today | Planned |
|---|---|---|
| CAS index, action cache, farm time | `MetaState` behind `RaftMetaLog`, applied from a single-voter Raft log on local disk (`--meta-dir`, or `<hot store>/meta`); behind `MemoryMetaLog`, in the process's memory, only in tests | the Raft log with three voters |
| Blob bytes | segments in an object store (in memory, or an S3 bucket) | the same, with garbage collection and multiple stores |
| Leases, operations, workers | `Scheduler` in the server's memory; a control record "commits" when appended | the same state machine fed from a replicated control log, kept on the servers' local disks |
| Memory kills | the scheduler's `MemoryState`, in the server's memory: each action key's memory floor and the order the floors were raised in, each held operation's memory-killed runs and busy-node reruns, each node's busy-node kill count; it compares with `Eq` | the same, inside the machine fed from the replicated control log |
| Callers, started leases, node registry | `FarmMachine` (`kbf-server`'s `machine` module) in the server's memory: each caller's record, the finished operations whose callers are kept for the finished retention, the leases whose `Start` was sent, and each node that has registered with whether its newest stream is open and its newest `NodeStatus`; each caller's channel and each node's stream stay beside it in the farm core, which tells the machine when a stream has closed | the same machine fed from the replicated control log |
| Node reports | read at `Hello`: `cpus`, `mem_gib` and `gpu` are booked; `arch`, `cpu.features`, `os` and the other exact keys are matched against each action's platform; held in memory, so a restart forgets every node | operator labels, report changes noticed by hash; the node registry kept durable through the replicated control log, with alerts on absent nodes |

Under `MemoryMetaLog` (tests only; the binary has no flag for it) the index is in memory,
so a restarted server forgets every blob, and an S3 store gets a fresh key prefix at
each start so it never reads objects a previous process wrote. The binary always runs
the Raft log: a restart reopens the log in its directory and keeps the index and the
action cache, and every start writes under the prefix the log committed when it was
created, `<cold store prefix><log id>/`, each in a new writer epoch.

Every piece of state sits behind a trait or a pure state machine, so moving it into a
replicated log changes the implementation behind the seam, not its callers:

- `MetaLog` (in `kbf-front`): `commit` a metadata command, `query` the committed
  state. `MemoryMetaLog` is the single-process implementation; `RaftMetaLog` (in
  `kbf-server`) implements the same two calls over a single-voter Raft log.
- `ObjectStore` (in `kbf-objstore`): five calls any S3-compatible store can serve, and `may_hold`, which a local store answers from its files and any other with yes.
- `Dispatch` (in `kbf-front`): how the REAPI front reaches the scheduler.
- `FarmMachine` (in `kbf-server`): a pure struct that compares with `Eq`; it reads
  no clock and holds no hashed map, which clippy checks for the module.
  `MemoryState` (in `kbf-sched`, read through `Scheduler::memory_state`) is the
  scheduler's first part that compares with `Eq`.
- `StateMachine` (in `kbf-types`): inputs in, effects out. The server carries out the
  scheduler's `Commit`, `Start` and `Answer` effects. The one place a control record
  is appended and fed straight back as committed is the step a replicated log
  replaces.

## One endpoint, a leader and its followers

Today kbf runs as one `kbf-server` process. The shape decided for more than one
server (**planned**) is in [deployment-topology.md](docs/design/deployment-topology.md),
with what exists today and the probes still to run. In short:

- **Servers on storage hosts,** each at a fixed address with a stable DNS name;
  dedicated storage hosts are preferred, and a converged layout (servers, the object
  store and daemons on the same hosts) is supported. Durable state is a Raft log on
  each server's local disk, with its snapshots also copied to the object store: one
  voter first, then three voters on three hosts for high availability (one leader,
  two followers). Elastic or stateless servers are not part of the design.
- **One name for the farm.** Bazel and Buck2 are configured with one remote address,
  and Buck2 sends everything to it. A tailnet ingress with a constant name and an
  automatic certificate terminates the clients' TLS; an HTTP/2 proxy behind it
  health-checks the servers and routes by gRPC method: reads and uploads to any server
  ready to serve reads, every other call to the leader. Each server reports both
  answers on two readiness paths and on `grpc.health.v1` on its REAPI listener (built
  today with `/readyz`'s one answer, [api.md](docs/api.md#grpchealthv1-on-the-reapi-listener)). The
  proxy reaches each server over TLS with a certificate from the farm's internal CA
  (the REAPI listener's TLS is built today), or in plain text on loopback when it runs
  on the same host (see [Security model](#security-model)).
- **Any server serves reads; the leader does the rest.** At three servers, every
  server answers the read path (`ByteStream.Read`, `BatchReadBlobs`,
  `FindMissingBlobs`, `GetActionResult`) from its replicated state and accepts upload
  bytes, which it writes to the object store before sending the leader the small
  metadata commit. So the byte traffic is shared across the three hosts. A follower
  may answer a false "missing", which only makes the client upload again, but never a
  false "present". The leader of the control log keeps every metadata write,
  `Execute` and `WaitExecution`, and every worker stream, so one place sees all free
  room on all workers and places every action. A follower takes over when it is
  elected. The first deployment is a single server.
- **Daemons dial the servers directly.** A daemon's worker stream does not go through
  the front: it is given the servers as one DNS name with a record per server, or a
  list, dials them over mutual TLS and holds one stream, to the leader. A follower
  ends a daemon's session with a status naming the leader, or answers `UNAVAILABLE`
  when it knows none, and the daemon tries the next server; it retries forever, with
  a bounded backoff (the list, the retry and the backoff are built today; the
  redirect is not). A failover keeps running leases: the lease epoch names the
  replicated log, and the daemon resends its results to the new leader. A daemon's
  blob reads and writes go to the worker listeners too (`--cas`), not through the
  front, and its reads are spread across all the servers.
- **Locality comes from placement.** The front sees a request's method but not what
  it is about, so kbf gets locality inside: daemons report which inputs they hold,
  and placement prefers a worker that already has an action's inputs.

What already holds for this shape in the single-server code: the seams above, lease
ids that carry the granting process's term (the Raft term of the metadata log), a
scheduler that refuses results from leases it does not hold, a lease epoch in
`Welcome` that makes a daemon drop leases a restarted server never granted (with
the Raft log the epoch is the log id, the same at every start, so the daemon keeps
its runs, and until the scheduler's state is in the log a restart leaves them running
as orphans whose results are refused), and a daemon protocol in which only the newest
stream of a worker counts. See [scheduler.md](docs/design/scheduler.md#more-than-one-server).

## Security model

Build clients and daemons reach `kbf-server` by different paths, and each path has its
own protection.

**Build clients go through a front.** TLS for Bazel and Buck2 ends at a front that
holds a real certificate for the farm's client-facing name: a load balancer, or a
proxy on a WireGuard mesh such as `tailscale serve` in its HTTPS mode. Behind the
front, `kbf-server` serves REAPI either as plain-text gRPC, bound to loopback (front on
the same host) or to the mesh interface, whose traffic WireGuard already encrypts, or
over TLS of its own (`--reapi-tls-cert`, `--reapi-tls-key`, built today) with a
certificate from the farm's internal CA. That TLS is for the hop from the front's proxy
to a server on another machine, and for clients that trust the internal CA and dial a
server directly. This is adequate for a private network; it does not replace the
front's certificate for the client-facing name, and it authenticates no caller. With
several servers (**planned**), the front is a tailnet ingress followed by an HTTP/2
proxy that routes reads and uploads to any ready server and every other call to the
leader ([deployment-topology.md](docs/design/deployment-topology.md)); that pair is
not yet probed.

- **Today:** the REAPI listener (`--listen`, default `127.0.0.1:8980`) serves plain
  text, or TLS when given a certificate and key (`--reapi-tls-cert`,
  `--reapi-tls-key`, both or neither). Under TLS it presents a server certificate
  only: it asks clients for none, so a client certificate identifies no caller. A key
  that does not match its certificate stops the server before it prints its start
  line. Over TLS as over plain text, it runs the authentication policy and the
  per-call authorizers of `--reapi-auth-policy`, in Buildbarn's model
  ([docs/reapi-auth.md](docs/reapi-auth.md)); the policies built so far (`allow`,
  `deny`, `any`, `all`, and instance-name prefixes) check no credential. Without the
  flag every call is accepted, and whoever reaches the port can read action inputs
  and outputs, write the CAS and Execute actions.
- **Planned:** credential policies in the same file (a bearer JWT in the
  `authorization` header, a remote authentication service), checked by `kbf-server`
  itself behind the front; the front passes the header through and does not check
  it. A peer address does not identify a caller here: a proxy on the same host
  connects from loopback, whoever its client is.
- **Built: TLS is optional; plain text off loopback is warned about, not refused.**
  `kbf-server` serves a listener without its TLS flags in plain text on whatever
  address it is given. When the REAPI or worker listener serves plain text on an
  address that is not loopback (a wildcard such as `0.0.0.0` or `::`, or any
  interface address; `::ffff:127.0.0.1` counts as loopback), the start line ends with
  one ` warning=plaintext-off-loopback:<listeners>` token, `<listeners>` being
  `reapi`, `worker` or `reapi,worker`, and the server starts. Whether plain text is
  acceptable on that network (a loopback front, a mesh interface WireGuard already
  encrypts) is the operator's call. A deployment that wants TLS, ours included, turns
  it on in its config: `--reapi-tls-cert` and `--reapi-tls-key` for a REAPI hop that
  crosses machines, and the three worker TLS flags for daemons on other machines.
  The warning looks at the bind address only: it does not restrict which peers
  connect, and it does not look at `--reapi-auth-policy`.
- **A front that works: `tailscale serve` in HTTPS mode.** A probe with
  `tailscale serve` 1.102.4 ran its HTTPS mode (`tailscale serve --https=<port>
  http://127.0.0.1:<reapi>`) in front of a plain-text REAPI listener on loopback.
  That version has no `h2c://` backend scheme; the `http://` backend carried gRPC.
  Through it, over TLS, these all worked: GetCapabilities, FindMissingBlobs, a 64 MiB
  ByteStream write and read back with a matching sha256, BatchUpdateBlobs and
  BatchReadBlobs (up to 1 MiB each), Execute, and WaitExecution on a finished action.
  A Buck2 remote-only build got through capabilities, uploads and Execute (the probe's
  fake driver wrote no outputs, so the action itself failed on missing outputs).
  Configuring `tailscale serve` needs root or a Tailscale operator on the host.
  `kbf-server` sees every such connection as coming from loopback. The HTTPS mode can
  add Tailscale identity headers (such as `Tailscale-User-Login`; not tested by the
  probe); `kbf-server` does not read them.
- **Not supported for REAPI: `tailscale serve`'s TLS-terminated TCP mode**
  (`--tls-terminated-tcp`). Its certificate verifies, but it negotiates no ALPN, and
  gRPC clients require `h2`. Buck2 fails with "HTTP/2 was not negotiated"; gRPC's C
  core (for example Python `grpcio`) fails with "Cannot check peer: missing selected
  ALPN property".
- **Long silent streams.** Through the HTTPS front above, a queued Execute and a
  WaitExecution stream for an action no node could run carried no messages for 300 s;
  both then ended with `FAILED_PRECONDITION` when `--unservable-wait-secs=300` failed
  the action. That front did not cut them. Another front whose idle timeout is shorter
  than the longest queue wait would cut these streams.

**Daemons do not go through the front.** A daemon's `--server` (repeatable; each
host resolved again on every round of connection attempts, which never stop) and its
`--cas` both name the worker listener, not the client front above. The worker listener keeps its own mutual TLS end to end. With
several servers (**planned**), daemons dial the servers' own names, with no balancer
in between, a follower redirects them to the leader, and they retry forever
([deployment-topology.md](docs/design/deployment-topology.md)).

- Daemons connect only over mutual TLS (`https://` URLs; the daemon refuses anything
  else). The server's worker listener serves mutual TLS when given a certificate, key
  and client CA. A daemon's certificate must name its node id as its one DNS
  subjectAltName, so a certificate can speak only for its own node, and a deny list
  (serials, public keys, node ids), looked at again at every `Hello`, `Heartbeat`, `Result`, `NodeStatus` and blob call, refuses
  leaked or retired certificates without a restart. There is no CRL or OCSP; short
  certificate lifetimes bound what the list misses. See
  [worker-protocol.md](docs/design/worker-protocol.md#node-identity-and-the-deny-list).
- **Built: blobs on the worker listener.** The worker listener also serves
  `ByteStream` (Read, Write, QueryWriteStatus) and `ContentAddressableStorage`
  (FindMissingBlobs, BatchUpdateBlobs, BatchReadBlobs, GetTree) over the farm's cache. A daemon's real
  drivers, and the integration cell's daemon, read inputs and write outputs there:
  `--cas` must be an `https://` URL, normally the same as `--server`, and the daemon
  presents the same certificate as on its worker stream; plain `http://` is refused
  at start. Each blob call is admitted on its own: the certificate must carry exactly
  one DNS subjectAltName (the node), and neither the certificate's serial or public
  key nor that node may be on the deny list, which is looked at again for every call
  (its metadata at each call, the file read again only when that changed). A denied
  daemon is refused PERMISSION_DENIED from its next blob call, on a connection
  already open too. Nothing else of REAPI is served there (Execute, the action cache and
  `Capabilities` answer UNIMPLEMENTED), and on a
  plain-text worker listener every blob call is refused UNAUTHENTICATED. So a daemon
  needs neither the front nor a REAPI credential. See
  [worker-protocol.md](docs/design/worker-protocol.md#blobs-on-the-worker-listener).
- **Not built:** a check that a blob call's node is registered or connected, or that
  it reads only the inputs of its own leases; any certificate the deny list does not
  refuse can read every blob whose digest it names, and write blobs.

**Inside the farm:**

- Only the daemon path writes the action cache, and the metadata state machine itself
  refuses an action-cache write from any role but `Daemon`.
- Actions run without network (`--network=none`) in rootless containers, as described
  in [daemon.md](docs/design/daemon.md).

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Testing

The project's testing rules are in [CONTRIBUTING.md](CONTRIBUTING.md): every test has
been seen failing on a planted defect (a mutant), and the coverage ratchet in
[coverage-baseline](coverage-baseline) never lets the count of uncovered lines or
branches rise. How the code is tested:

- **Pure cores, unit-tested.** The scheduler, metadata, segment, capability and Raft
  crates are plain state machines with plain tests; each test says which defect it
  catches.
- **Deterministic simulation.** `kbf-sim` runs state machines on a virtual clock with
  a seeded random stream and a message bus that delays, drops, duplicates, reorders
  and partitions. The same seed gives the same trace hash on any machine, so a failing
  seed is a complete bug report. The scheduler runs there with a control log and
  workers that disconnect, reboot and restart (checks: every `Start` names a committed
  lease, every operation is answered once, no self-fenced operation runs twice). The
  Raft core runs there with crashes part-way through its effects (checks: election
  safety, log matching, leader completeness, state machine safety).
- **Contract suites.** Every object store backend must pass the conformance suite in
  `kbf-objstore`; CI runs it against the in-memory store, the local-directory store,
  and MinIO and RustFS containers, with and without Object Lock.
- **Real services.** Server tests run the REAPI and worker services over real gRPC,
  including mutual TLS. The container driver's tests run real rootless `crun` on
  hosted runners, on x86-64 and arm64.
- **End to end.** The `integration` workflow starts a cell (an S3 store, one
  `kbf-server`, one daemon) and builds sample projects with pinned Bazel and Buck2
  versions, remote-only, twice; the second build must be all remote cache hits. A
  Buck2 action that sleeps then holds a lease in flight while the node is drained
  through the operator API, which must list that lease. Its
  daemon uses a test-only runtime that runs actions as plain processes, so it proves
  the protocol and the cache path, not isolation.
- **Repository lints.** Workflows may only use hosted runners and pinned actions from
  an allowed set; public text may carry no machine addresses or home paths.
