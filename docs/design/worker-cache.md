# Worker inputs: a shared blob transfer engine and a per-node NVMe file cache

Status: design, revision 5 (one connection; the cache persists from its first PR, through one snapshot file that unknown files are adopted into). Stages C1 and T1 are built; where they differ from the text, "C1 as built" and "T1 as built" (end of section 10) say how.
Every "today" claim cites a file on `main` at `d7f25cf` (#524). Every number marked
*unmeasured* is an estimate that the PR introducing it must replace with a measurement.

This document covers four changes to how a kbf-daemon moves blobs:

1. bounded concurrent fetch and upload (fix 1);
2. FindMissingBlobs before upload (fix 2);
3. batched small blobs, BatchReadBlobs and BatchUpdateBlobs (fix 3);
4. a content-addressed file cache on each node's NVMe disk, hard-linked (Linux
   containers) or cloned (macOS) into each lease's input root (fix 4).

Fix 1 is in progress on the branch `daemon/parallel-cas` (stage A, section 9). Fix 2
arrives with the transfer engine (stage T1), which builds on A. Fixes 1 to 3, and later
the server's own object-store traffic, become one **shared transfer engine** (section
4). Fix 4 is the cache (sections 5 to 7). The cache does not depend on A or T1 to be
correct: it works over any `Cas` (section 9).

This is the design behind two lines that already promise it: `daemon.md` § Planned
("Prefetch and a local cache") and `storage.md` § Planned ("Locality").

---

## 1. What the code does today

| Step | Code | Behaviour |
|---|---|---|
| Input tree | `crates/kbf-daemon/src/tree.rs` `materialize` | A depth-first walk: one `fetch_message` per Directory, then **one awaited `fetch` per file, in series**. Files are created `O_EXCL` with mode 0644 or 0755. Both drivers call it (`kbf-driver-container/src/tree.rs` wraps it; `kbf-driver-native/src/runtime.rs:325`). |
| Blob connection | `crates/kbf-node/src/main.rs:456-472` `cas_client` | **One** `Endpoint::connect_lazy()` channel, which is one HTTP/2 connection, carries every blob of every lease. It refuses a `--cas` URL that is not `https://`. The worker stream has its own connection, built at `crates/kbf-daemon/src/daemon.rs:484` (`fn endpoint`). Neither sets HTTP/2 window sizes or a decode limit, so tonic's defaults apply. |
| Blob read | `crates/kbf-daemon/src/cas.rs` `CasClient::get` | One ByteStream `Read` per blob. Each blob is hashed when it arrives (`fetch`). No FindMissing, no Batch, no GetTree, no compression. |
| Blob write | `CasClient::put` / `put_chunks` | One ByteStream `Write` per blob, 1 MiB per message (`WRITE_CHUNK_BYTES`). Outputs are uploaded in series (`tree.rs` `collect`; container `outputs.rs`; native `kbf-outputs`). Every output is uploaded, even one the server already holds. |
| Worker listener | `crates/kbf-server/src/blobs.rs` | ByteStream (`WorkerBlobs`) and, since stage S1, the `ContentAddressableStorage` service (`WorkerCas`: FindMissing, Batch*, GetTree). Every call of both is admitted against the node certificate by one rule (`admit`, line 37). Under plain text every call is refused UNAUTHENTICATED. `Capabilities`, `Execution` and `ActionCache` answer UNIMPLEMENTED there. |
| Server batch | `crates/kbf-front/src/cas.rs` `batch_read_blobs`, `kbf-front/src/cache/batch.rs` `read_blobs` | Up to 1 GiB of blob data (`MAX_BATCH_TOTAL_BYTES`, `kbf-front/src/lib.rs`); the gRPC message cap of the CAS and ByteStream services is 1 GiB + 1 MiB (`MAX_MESSAGE_BYTES`; the other services keep `MAX_CONTROL_MESSAGE_BYTES`, 5 MiB). A batch read holds three copies of its data until sent and is admitted under `READ_BYTES_IN_FLIGHT` (6 GiB); the message bytes of calls in flight on both listeners are bounded by `MESSAGE_BYTES_IN_FLIGHT` (4 GiB, `kbf-server/src/inflight.rs`). Since stage S1 one metadata query looks up and touches the batch, blobs back to back in one segment come from one range GET, and up to 32 GETs run at once (`storage.md` § Reads). |
| Container lease | `crates/kbf-driver-container/src/runtime.rs:252-277`, `podman.rs` | `materialize` writes the input root `lease/root`, which becomes the **overlay lower** (`--volume=root:/kbf/root:O,upperdir=upper,workdir=work`). `prepare_outputs` **renames** every input that an output names from `root` into `upper` (`tree.rs:188`). Then `podman unshare chown -hR 1:1` runs over root, upper and work; after exit, `chown -hR 0:0` runs over the same paths (`runtime.rs:274-277,421-428`). Containers run with `--userns=nomap` (`podman.rs:136`), so the daemon's uid maps to nothing inside. |
| Native lease | `crates/kbf-driver-native/src/runtime.rs`, `lib.rs` | Writes the input root into the lease directory and runs the action **as the daemon's user**. On macOS a `sandbox-exec` profile lets the action write only under its lease directory. On Linux nothing is enforced. |
| Per-lease timing | `runtime.rs:322-341` (container), native `:487` | `input_fetch_start/completed_timestamp` cover all of prepare. Resource usage travels as an `auxiliary_metadata` Any (`kbf-daemon/src/usage.rs`). |
| Placement | `scheduler.md` § Placement | The first live, feasible worker in name order. No locality input. `Heartbeat` (`worker.proto:251`) carries `seq`, `report_hash` and `running`. Capabilities live in the Hello and are hashed (`report.rs`). |

**Stage A as of this revision.** The `daemon/parallel-cas` work is not pushed yet. In its
current form it fetches a lease's Directory messages a level at a time and its file
blobs up to 64 at a time and 256 MiB in flight (`FETCHES_IN_FLIGHT`,
`FETCH_BYTES_IN_FLIGHT`). It uploads up to 64 outputs at a time (`UPLOADS_IN_FLIGHT`),
each distinct digest once. These limits are constants **per call**, so 16 concurrent
leases can have 1,024 streams on the one connection. It has no FindMissing, because
the worker listener cannot answer one. Section 4 replaces the per-call limits with one
limit for the whole process.

**The zig case.** Every compile's input root carries the unpacked zig toolchain:
about 15,000 files: zig 0.12.0 for x86-64 Linux, the toolchain komira pins, unpacks to
15,189 files (144 of them empty) in 989 directories, 307 MB (C1 counted its release
archive; an input root carries the files the rule declares, at most these).
Today each file costs one network round trip per action, in series, because nothing
is kept between leases.

---

## 2. Prior art

The sources were read, not run or measured: Buildbarn bb-remote-execution `582d07ff`
and bb-storage `6ded598f`, BuildBuddy `c422445c`, NativeLink `a435beba`, REAPI
`6def1c5d`, buck2 `a8d6b29e`. A cell says "not found" when the research looked and saw
nothing; that is a search result, not proof.

| | Buildbarn | BuildBuddy executor | NativeLink | **kbf (planned)** |
|---|---|---|---|---|
| Worker cache form | Native: a flat hard-link directory (`hardlinking_file_fetcher.go`). Virtual (the recommended mode): FUSE or NFSv4 over a local block store. | Hard-link directory with 2-character prefix subdirectories (`filecache.go`) | `FastSlowStore`: a local `FilesystemStore` in front of the gRPC CAS | Content-addressed file directory, 2-hex-character subdirectories, on the same filesystem as the lease scratch |
| Into the action dir | `link()` | `os.Link`; a reflink only for anonymous users or with `local_cache_always_clone` | `hard_link_many`; a directory `clonefile` on macOS | Container: `linkat`. macOS: `clonefile`. Linux native: **no cache** (5.2b) |
| Across a restart | Hard-link cache **wiped at start** (`main.go` `RemoveAllChildren`). The block store persists only when `persistent` is set. | Rescanned; an optional wipe after an unclean shutdown | Not covered by the research | Kept from the first cache PR: `tmp/` cleared, an on-disk snapshot loaded (no scan before serving), unknown files adopted in the background, re-hashed on first use |
| Executable bit | A separate key, `+x`/`-x` | Separate entries | A private 0555 copy per digest | A separate key (`.x` suffix) |
| File mode | 0444/0555, fixed mtime | **0644 (writable)** | 0444/0555 | 0444/0555, owned by the daemon's uid, fixed mtime |
| What stops an action writing a cached inode | Native: modes, plus privilege separation **when configured** (`bb_runner` `run_commands_as`). The README calls the split what makes hard-link caching safe. Without it, a same-user action can `chmod` and write (the research's inference). Virtual: by construction (`EACCES` on open for write). | Nothing in the code | Read-only modes (files 0444); privilege separation not covered by the research | Container: overlay lower plus an unmapped owner. macOS: a CoW clone, plus a sandbox that denies the cache directory. Linux native: no cache until actions run as another uid. |
| Concurrent misses | Merged per key (`downloads` map) | Merged | Merged (`fast_slow_store.rs`) | Merged per key, in one process (one owner per cache directory) |
| Eviction | Count and bytes; RANDOM recommended, LRU when inode identity matters (Clang `#pragma once`) | LRU, bytes or a percentage of the disk (default 1 GB) | `max_bytes`/`evict_bytes`; without them, unbounded | LRU by bytes, plus a free-space floor that counts only reclaimable entries |
| Download concurrency | One limit per process, `input_download_concurrency` (examples: 10) | One limit per process, unlimited by default. A batch of N files holds N slots, so batches are capped at limit/8 files. macOS link tasks fixed at 5. | 64 per directory level, 64 for the whole directory cache | One limit per process in the transfer engine (section 4), and a separate limit for local links |
| Connections | Many ByteStream streams over one HTTP/2 connection | Not covered | Not covered | One gRPC HTTP/2 connection per daemon with many streams and explicit windows (4.2) |
| FindMissing before upload | Batches of up to 10,000 (100 with native directories, for file descriptors) | Chunks of 1,000 | Yes; 64 uploads at a time | Yes, chunks of at most 1,000 |
| Batch read/write | **Never sent by the client** (`grpcclients/cas_blob_access.go:394`) | Up to 2 MiB per batch | Off by default; 128 KiB blobs, 3 MiB batches, 4 in flight | Blobs up to 64 KiB, batches up to 2 MiB, after the server reads a batch in parallel (stage S1) |
| Empty files | Not covered | Created directly, with no lookup | Not covered | Created directly (6.5) |
| Inserts | Download into the action directory, then link back | Not covered | Temp file on the same disk, then rename; each write calls `sync_all()`, so writes are capped | Temp file, then a rename that never replaces (6.3); an unindexed name already there is re-hashed and adopted or unlinked first (6.9); no fsync |
| Directory-level cache | An in-memory Directory cache keyed `{digest, is_tree_root}` | `GetTree` with cached subtrees (a non-standard extension) | Whole-directory and subtree cache, `GetTree` prefetch | In-memory Directory cache, then subtree link plans (6.6) |

**The lesson.** Read-only modes stop nothing an action can undo. An action that runs as
the cache owner can `chmod u+w`, write, and `chmod a-w` again. The cases that hold are
the constructions that put the inode out of the action's reach: Buildbarn's FUSE mode,
and Buildbarn's native mode when `run_commands_as` gives actions another uid.
BuildBuddy's writable 0644 entries, and modes alone wherever actions share the owner's
uid, are open corruption paths. kbf's three drivers isolate actions three ways, so
section 5.2 settles each one separately. One of them, the Linux native driver, gets no
cache.

---

## 3. Contracts every stage keeps

1. **The server is the truth.** The local cache never answers FindMissing for an upload,
   as in Buildbarn's `ReadCachingBlobAccess` (Put and FindMissing go to the slow
   store). A blob the node holds but the server has collected is uploaded again.
2. **Every byte from the network is hashed before it is used**, batched or streamed,
   into memory or into a file (`digest_of`, as `fetch` does today).
3. **No cache flag means today's behaviour.** `--cache-dir` is unset by default. If the
   cache cannot be used, the daemon logs a warning, reports `cache_disabled_reason`,
   and runs uncached. It never refuses to serve over the cache (the "options plus at
   most a warning" ruling).
4. **Order of operations for one input file:** the local cache, then the cache's
   in-flight fill for that key, then the transfer engine (a batch if small, a stream if
   large).
5. **No caller waits for one blob before it asks for the next.** Callers hand the engine
   sets of digests (section 4).

---

## 4. The shared transfer engine

CEO ruling (2026-10-13): all bulk blob movement goes through one shared transfer engine
in kbf. That covers input fetch, output upload, cache fill, and later the server's
object-store traffic. The engine speaks gRPC over HTTP/2 to kbf-server and runs many
concurrent streams under one in-flight limit for the whole process. Small blobs are
batched and large blobs are streamed in chunks. FindMissing runs before every upload.
Never one file at a time. CEO decision (2026-10-13): **one** HTTP/2 connection per
daemon, carrying many concurrent streams (4.2).

### 4.1 Where it lives and what it replaces

- A new crate, `kbf-transfer`. Its **core** knows nothing about REAPI: the permit pool,
  the byte budget, queues, merging duplicate requests, and counters. The **REAPI
  backend** in the same crate speaks ByteStream and CAS to kbf-server. One engine is
  built per daemon process (in `kbf-node`, where `cas_client` is today) and shared as an
  `Arc` by every lease, every driver and the cache.
- `CasClient` becomes the engine's wire client over its one connection and stops being
  what a runtime holds. The engine's lease scope implements the existing `Cas` trait, so the
  first PR changes no driver and keeps every test that uses `MemoryCas`.
- Stage A's per-call constants (`FETCHES_IN_FLIGHT`, `FETCH_BYTES_IN_FLIGHT`,
  `UPLOADS_IN_FLIGHT`) are removed in the engine PR, not kept beside it. `materialize`
  and the upload walks hand the engine whole sets. No second limit survives to drift.
- **Later, server to object store (stage T3, not designed here).** kbf-front's segment
  reads and writes against MinIO move onto the same core with an object-store backend:
  its own HTTP connections, one in-flight limit, and coalesced range GETs. S1's
  parallel batch read (stage S1, section 9) is a small bounded join inside `batch_read_blobs`.
  T3 replaces it; that migration is planned, not left as a second mechanism.

### 4.2 One connection, many streams

- **One connection.** The engine opens one `Endpoint::connect_lazy()` channel to the
  `--cas` URL, which is one HTTP/2 connection, as `cas_client` does today. Every request
  of every lease is a stream on it. HTTP/2 multiplexes, so with the windows below one
  slow stream does not hold up another's flow control. What bounds the work is the
  process-wide in-flight limit (4.4), not a connection count.
- **A reset.** If the connection drops, every stream in flight on it fails at once. The
  engine reconnects with backoff and retries each unfinished request (4.4); a read
  resumes from its offset.
- **HTTP/2 windows are set explicitly**: `--transfer-stream-window-bytes` (default 8 MiB,
  *unmeasured*) and a connection window of eight times that. The default 64 KiB
  stream window caps one stream at about window/RTT. No setting in the code changes it
  today.
- **The decode limit is set explicitly**: `max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)`
  on the client. tonic's 4 MiB default would refuse a full server batch once
  framing is added.

### 4.3 API

```rust
/// One per daemon process.
pub struct Transfers { /* connection, permits, byte budget, in-flight map, counters */ }

impl Transfers {
    pub fn new(endpoint: Endpoint, config: TransferConfig) -> Self;
    /// A lease's view: its own counters, and its queued work cancelled on drop.
    pub fn scope(&self, lease: &LeaseId) -> Scope;
    pub fn stats(&self) -> EngineStats;
}

impl Scope {                                   // also implements `Cas`
    /// Blobs no larger than `--transfer-memory-blob-bytes`, in completion order,
    /// each one hashed. Duplicate digests are fetched once.
    pub fn read_many(&self, digests: Vec<Digest>)
        -> impl Stream<Item = Result<(Digest, Bytes), CasError>>;
    /// One blob streamed into `file` chunk by chunk, hashed as it is written.
    pub async fn read_to(&self, digest: &Digest, file: &mut File) -> Result<(), CasError>;
    /// FindMissing over every blob, then uploads only the missing ones.
    pub async fn upload_missing(&self, blobs: Vec<Upload>) -> Result<UploadReport, CasError>;
    pub fn stats(&self) -> TransferStats;
}

pub enum Upload { Bytes(Digest, Bytes), File(Digest, PathBuf) }
```

- **`read_many`** sorts the request. Blobs no larger than `--transfer-batch-max-blob-bytes`
  go to a batch queue, once batching is on (T2). Everything else becomes ByteStream
  reads. A batch is sent when it reaches `--transfer-batch-bytes`, when the caller's
  set is exhausted, or after a short linger (2 ms, *unmeasured*), whichever comes
  first. A batch holds **one** permit, so a 500-blob batch cannot starve the streams.
  That is BuildBuddy's lesson, solved by counting a batch as one request instead of
  capping its size.
- **Duplicates are merged in memory.** Concurrent `read_many`/`read` calls for one digest,
  from any lease, share one network fetch, and the `Bytes` are shared. `read_to` is not
  merged across callers: two leases streaming the same large blob to two files fetch it
  twice when there is no cache. With a cache, the cache merges fills per key (6.4), so
  each large blob is fetched once per node. There are two maps because they hold two
  different things, in-memory results and on-disk entries. No blob is fetched twice
  while the cache is on.
- **`upload_missing`** sends FindMissing in chunks of at most `--transfer-find-missing-chunk`
  (default 1,000) digests. Missing blobs up to the batch threshold go to
  `BatchUpdateBlobs` (T2); the rest stream through ByteStream `Write` in
  `--transfer-chunk-bytes` messages (default 1 MiB, today's `WRITE_CHUNK_BYTES`). An
  upload from a file reads at most the size that was hashed, so a file still being
  appended to cannot make the upload disagree with its digest (Buildbarn's
  `UploadFile`). `UploadReport` says how many blobs and bytes were present, batched and
  streamed.
- **Before the server can answer.** The worker listener answers FindMissing and Batch
  with UNIMPLEMENTED until stage S1 is deployed. The first UNIMPLEMENTED turns that
  call off for the life of the process: it is logged once and counted in
  `find_missing_unavailable` or `batch_unavailable`. FindMissing then degrades to
  "upload everything" (today's behaviour), and batches to ByteStream. The engine ships
  before the server work with no flag day.
- **Limit discovery.** At first use the engine calls `GetCapabilities`. It takes the
  smaller of the advertised `max_batch_total_size_bytes` (where nonzero) and
  `--transfer-batch-bytes`. It never assumes 4 MiB. The worker listener does not serve
  `Capabilities` (stage S1 adds only the CAS service), so there the call answers
  UNIMPLEMENTED: the engine then uses `--transfer-batch-bytes` alone, logs that once,
  and does not ask again for the life of the process. The flag's default (2 MiB) is
  under the server's 4 MiB batch limit, and a deploy that raises the flag past it gets
  INVALID_ARGUMENT for the batch, which is resent as ByteStream (4.4). Serving
  `Capabilities` on the worker listener is not planned.

### 4.4 Limits, retries, cancellation

- **One in-flight limit for the process:** `--transfer-in-flight` (default 64,
  *unmeasured*; Buildbarn's examples use 10, NativeLink 64, BuildBuddy unlimited). It
  counts concurrent requests: a ByteStream read or write, a batch call, a FindMissing
  call. Permits are granted first come, first served (`tokio::sync::Semaphore` is
  FIFO), so a big lease does not starve a small one past queue order. Uploads (a
  ByteStream write, a `PutRange`) take a permit of the uploads' share first, all but a
  quarter of the limit (48 of 64): the server holds a write until it is durable, which
  on a slow store is minutes, and without the share the permits drain to the writes
  and one lease's upload stops every other lease's reads and cache fills (measured on
  a farm node: nine leases' inputs waited 22 minutes; issue #641).
  `--transfer-lease-in-flight` (unset by default) can cap one lease's share if the
  farm's metrics show a need.
- **One byte budget:** `--transfer-bytes-in-flight` (default 256 MiB, as stage A's
  constant). It bounds bytes held in memory: batched blobs, and `read_many` results not
  yet taken. A streamed read holds only its chunk buffers. A blob larger than the
  budget is fetched alone.
- **Local link and copy work is not in the engine.** Cache hits never wait behind network
  permits. They take the cache's own `--cache-link-concurrency` (6.4).
- **Retries.** A per-blob UNAVAILABLE, or a connection reset, is retried with backoff
  (after the reconnect, for a reset), up to `--transfer-retries` (default 3). A ByteStream read resumes
  from the bytes already hashed (`read_offset`). A write restarts from 0; resumable
  writes wait for `QueryWriteStatus` on the server. NOT_FOUND is `CasError::Missing`,
  the client's FAILED_PRECONDITION, and is never retried. A per-blob status inside a
  batch is handled per blob (REAPI). An INVALID_ARGUMENT for a whole batch is a bug:
  logged, then that batch is resent as ByteStream.
- **Cancellation.** Dropping a `Scope` (a lease killed mid-prepare) removes its queued
  requests that have not started. A started fetch other callers are waiting on keeps
  running. One with no waiters left is cancelled, unless more than half its bytes have
  arrived (then it finishes, for the next lease or the cache). Daemon shutdown aborts
  everything.

### 4.5 Knobs: flags, not environment variables

| Flag | Default | Meaning |
|---|---|---|
| `--cas` | (required by drivers) | The worker listener URL (4.7) |
| `--transfer-in-flight` | 64 | Requests in flight, for the whole process |
| `--transfer-lease-in-flight` | unset | Optional cap for one lease |
| `--transfer-bytes-in-flight` | 256 MiB | Memory held by in-flight and untaken blobs |
| `--transfer-memory-blob-bytes` | 1 MiB | Largest blob `read_many` returns in memory |
| `--transfer-batch-max-blob-bytes` | 64 KiB | Largest blob put in a batch (0 = no batching) |
| `--transfer-batch-bytes` | 2 MiB | Largest batch, capped by the server's advertised limit |
| `--transfer-chunk-bytes` | 1 MiB | ByteStream write message size |
| `--transfer-find-missing-chunk` | 1,000 | Digests per FindMissing call |
| `--transfer-stream-window-bytes` | 8 MiB | HTTP/2 stream window; the connection window is 8x |
| `--transfer-retries` | 3 | Retries per blob on UNAVAILABLE or a reset |

Every default marked *unmeasured* in this section is set from T1's measurement and
written into the farm's deploy config, not hard-coded as policy.

### 4.6 Metrics, through the API

Per lease, the `Scope` writes `kbf.worker.v1.TransferStats` into `auxiliary_metadata`,
next to `ResourceUsage`: `read_calls` (ByteStream), `batch_read_calls`, `batch_blobs`,
`read_bytes`, `merged_reads` (duplicates served by another caller's fetch),
`find_missing_calls`, `outputs_present` (skipped by FindMissing), `write_calls`,
`batch_write_calls`, `written_bytes`, `retries`, `permit_wait_ms`, `transfer_ms`.
These reach agents through the invocations API's per-action view, which already
records `ExecutedActionMetadata`, and are summed by the invocation's metrics
(`GET /v1/invocations/{id}/metrics`, `invocations.md` §4.4). Adding the sums to that
response is part of T1.

Per process, on the operator API: `connection_ready`, `requests_in_flight`,
`queue_depth`, `bytes_in_flight`, `connection_resets`, `find_missing_unavailable`,
`batch_unavailable`, and read and write byte rates. None of this goes in the
Heartbeat.

### 4.7 TLS

kbf must not enforce TLS (CEO ruling: TLS optional in the open-source build; the farm's
deployment uses it). The engine has no TLS policy: it connects to whatever `--cas`
names. Today three places refuse plain text on blob traffic: `cas_client`
(`kbf-node/src/main.rs:463`), `Server::parse` (`kbf-daemon/src/connect.rs:57-58`), and
the worker listener's `Peers::admit_call` (`kbf-server/src/identity.rs:184-190`).
Lifting them is its own small PR (stage P), outside this design.

---

## 5. How a cached file reaches a lease, per driver

kbf's three drivers isolate actions in three different ways, so each driver gets its
own answer.

### 5.1 Container driver (Linux, rootless Podman): hard links into the overlay lower

`lease/root` stays the overlay lower. Its **directories are fresh per lease**, created by
`materialize` as today. Its **files become hard links to cache entries**
(`linkat(cache, root/...)`). Two independent mechanisms keep the action from changing a
cached inode:

1. **Overlayfs never writes the lower layer.** Any write, truncate, chmod, chown or xattr
   change on a lower file copies it up into `upper`, and the lower inode is untouched.
2. **The owner is unmapped.** Under `--userns=nomap` the daemon's uid maps to nothing in
   the container, so a cached file appears owned by the overflow uid (65534). The
   container's root has no `CAP_DAC_OVERRIDE` or `CAP_FOWNER` over an inode whose owner
   it does not map, so it cannot open the file for writing or chmod it. It **can** read
   and execute it (the "other" bits are r-x). It can also unlink or rename the name,
   which leaves a whiteout in upper and drops nothing from the cache.

The action never sees the cache directory itself: it is not mounted into the container.

Three code changes are required, or the guarantee is void:

- **`chown -hR` must never touch a file in `root`.** Today `podman unshare chown -hR 1:1
  root` would re-own every cached inode on every lease, and the hand-back would re-own
  it again. The fix chowns **directories only** in `root`, plus all of `upper` and
  `work` as today. `podman unshare chown` has no type filter, so this becomes
  `podman unshare <kbf-node> chown-dirs 1:1 <root>`: a pure-Rust subcommand of the node
  binary that walks by descriptor without following symlinks (the rules of `remove.rs`).
  The hand-back to 0:0 does the same over `root`.
- **No cache inode may ever be under `upper`.** Today `prepare_outputs` `rename`s each input
  that an output names from `root` into `upper`. A renamed link would still be owned by
  the unmapped uid, so the action could not write it directly. The real danger is the
  next step: `chown -hR 1:1` over **upper**, which the design keeps, would re-own the
  shared inode to an id the container maps, and then the action could write it. The
  fix: an input that an output names is materialized as a **private copy** (copy or
  `FICLONE`), never a link. That includes every file in a moved subtree.
  `materialize` takes the output path set and copies those paths. C-2 plants the
  mutant: keep the rename, with the upper chown in place.
- **The owner stays the daemon's uid.** A cache file owned by subordinate id 1 would let
  an action open an input `O_RDWR` and copy it up, as it can today. But
  `fs.protected_hardlinks=1` (the default on Ubuntu, Debian and Fedora) refuses
  `linkat` on a file the caller does not own unless it can read and write it. The
  daemon could not link such files without entering the user namespace once per link.

**Behaviour change.** An action that opens an input file for writing in place gets EACCES
(today it gets a copy-up). REAPI lets a worker refuse this. Buildbarn's virtual mode
returns EACCES on open for write. Buildbarn native with `run_commands_as` refuses the
same way, by ordinary permission checks (inferred, not tested). BuildBuddy and NativeLink
without privilege separation do **not** refuse: their actions can chmod and write. An
output that is also an input stays writable, through the private copy. Open question
Q1 asks whether to accept this.

**What C-1 must prove on the farm before C1 merges.** The overlay behaviour above is
known for kernel overlayfs. C-1 records the graph driver (`podman info`: kernel overlay
or fuse-overlayfs) and the mount options (`index`, `userxattr`, `metacopy`). It then
checks both properties on that driver: the lower is never written, and a write to an
unmapped-owner file is refused. The refusal's errno may differ from plain permission
checks. `podman.rs:24-29` notes that writes under an unmapped-owner directory fail with
EROFS. Any errno is acceptable if the lower is unchanged. On a graph driver C-1 has not
passed, the daemon warns and runs uncached (`cache_disabled_reason=graph-driver`).

### 5.2 Native driver

**(a) macOS: `clonefile`, never a hard link, inside a sandbox that denies the cache.**

Actions run as the daemon's user, and the sandbox profile allows writes anywhere under
the lease directory (`kbf-driver-native/src/lib.rs`). A hard link in the lease
directory would therefore let an action write, chmod or `chflags` the cached inode.
`clonefile(2)` on APFS makes a new inode that shares blocks copy-on-write: a write
changes only the clone, for about the price of a link.

The clone protects the entries an action can see. What protects the cache directory
itself is the sandbox: the profile allows no writes outside the lease directory, so
`--cache-dir` must lie outside every lease directory. Test C-3b proves an action
cannot write, chmod or `chflags` a cache entry by its cache path. Without that
profile the cache is off (`cache_disabled_reason=no-sandbox`).

More on macOS:

- The cache and the scratch directory must be on one APFS volume, or `clonefile` returns
  EXDEV and the daemon copies.
- `remove_tree`'s permission and flag restoration (`kbf-outputs/src/remove.rs`) touches
  only lease-private inodes, as today.
- The call goes through the `libc` crate's `clonefile` binding, which keeps it pure Rust.
- The cache directory gets `.metadata_never_index` (no Spotlight indexing of millions of
  0444 files). It also gets the backup-exclusion attribute that
  `CSBackupSetItemExcluded` sets, so Time Machine skips it.
- The default `--cache-link-concurrency` on macOS is 5 (BuildBuddy fixes macOS link tasks
  at 5; NativeLink's comment says more than 64 per level floods the metadata lock). C2
  measures it.

**(b) Linux: no cache until actions run as another uid.**

A Linux native action runs as the daemon's uid, with nothing enforced. It can
`chmod u+w` a cache entry, rewrite it at the same size, `chmod a-w` it and reset its
mtime with `utimensat`. Owner, mode, size and type checks all pass afterwards. Cloning
or copying into the lease does not help: the cache directory itself is writable by the
action. So with the Linux native driver, `--cache-dir` is ignored: the daemon warns
and runs uncached (`cache_disabled_reason=native-same-uid`). The cache becomes possible
once actions run as a uid other than the cache owner's (Buildbarn's `run_commands_as`
split), which is not planned here.

A kernel-enforced alternative is worth a probe, but nothing here relies on it:
fs-verity (`FS_IOC_ENABLE_VERITY`, ext4 with the `verity` feature, or btrfs). It makes
a file's contents immutable while still allowing links. It is unverified, and a probe
target on the farm tests it first.

**A same-host rule.** A Linux native action running under the uid that owns a container
cache can write that cache directly, whether or not the two daemons share it. The deploy
rule is that the Linux native driver never runs under a uid that owns any `--cache-dir`.
The farm runs only the container driver on Linux.

---

## 6. The cache

### Who writes the cache

Only the daemon. A fill downloads the blob into `tmp/`, which only the daemon can reach,
hashes it while writing and checks the digest, chmods it 0444 (0555 if executable), and
renames it into `blobs/`. Eviction is the daemon unlinking a name. Nothing ever opens a
file under `blobs/` for writing. Actions only read: they get hard links (containers) or
clones (macOS) of cache files, and they run as a uid that cannot write those files (in a
container the files' owner is unmapped; on macOS the clone is a separate inode and the
sandbox denies the cache directory). A fill that finds an unindexed name whose bytes do
not match its key unlinks that name before it fetches (6.9). The index (6.9) is written by
one daemon thread, and the background adopt pass only reads the directory: it never
deletes a file.

### 6.1 On-disk layout

```
<cache-dir>/                     --cache-dir; same filesystem as --scratch
  lock                           exclusive flock: one owning daemon; never unlinked
  v1/                            mode 0700; an unknown layout version is moved aside, never read
    blobs/<h0h1>/<sha256>-<size>     non-executable entry, mode 0444
    blobs/<h0h1>/<sha256>-<size>.x   executable entry, mode 0555
    tmp/<random>                     fills in progress
    index.snap                       the index: every entry's key, size, last use (6.9)
    index.snap.tmp                   a snapshot being written; cleared at start
```

- **The lock comes first.** At start the daemon opens `<cache-dir>/lock` (creating it if
  absent) and takes the exclusive `flock` **before it touches anything else** under
  `<cache-dir>`: clearing `tmp/`, loading the index, and moving an unknown layout
  version aside all happen only while the lock is held. The lock file sits outside `v1/`
  so that no cleanup can remove it, and the daemon never unlinks it. Otherwise a second
  daemon could delete the first's lock file, create a new one, lock that, and both
  would own the directory.

- **The key is the digest plus the executable bit.** Hard links share mode bits, so one
  inode cannot be both 0444 and 0555. A fill of `k.x` when `k` is present is a local
  copy, not a network fetch.
- **256 subdirectories** keep each directory small (BuildBuddy's comment says the prefix
  helps throughput on many-core machines).
- **`v1/` is 0700.** Entries reach leases through their links, never through the cache
  path, so no other host user needs to list it. Entries stay readable by "other",
  because the container sees them as unmapped-owner files.
- **Owner and mode.** Every entry is owned by the daemon's uid, mode 0444/0555, with mtime
  set on insert to one fixed constant, `FIXED_MTIME` (Buildbarn's
  `DeterministicFileModificationTimestamp`).
- **Same filesystem.** At start the daemon compares `st_dev` of `--cache-dir` and
  `--scratch`. If they differ, it warns and runs the cache in **copy mode**: it still
  saves the network fetch, but copies instead of linking (`link_mode=copy`).
- **The name must not start `kbf-lease-` or `lease-`**: both drivers' sweeps remove entries
  with those prefixes (the `LEASE_PREFIX` filter in `sweep`,
  `kbf-driver-container/src/runtime.rs:630`; native `sweep.rs`). The
  daemon checks the name and warns.

### 6.2 What a lease sees: behaviour changes

1. **Identical inputs share an inode.** Within one lease, each key resolves to **exactly
   one inode**. If linking fails for a key (EMLINK, EXDEV, a lost race with eviction),
   the daemon makes **one** private copy for that lease and links the key's other
   names in the lease to it. Tools that read `(dev, ino)` then see the same answer on
   every run of identical inputs: GNU tar and cpio hard-link records, `rsync -H`, `du`,
   Clang `#pragma once`. Today every input is its own inode. With the cache, the
   identity is a function of the input tree alone.
2. **The owner shows as 65534** inside a container (5.1). Today it shows as 1.
3. **mtime is a constant.** Today it is the write time. Tools that compare input mtimes,
   such as make-style tools, see a constant.
4. **In-place writes to inputs get EACCES** in a container (5.1, Q1).

Without `--cache-dir`, none of this changes.

### 6.3 Integrity

| Threat | Defence | Cost |
|---|---|---|
| An action writes through a cached inode | By construction. Container: overlay lower plus unmapped owner, and the cache is not mounted. macOS: CoW clone, plus a sandbox that denies the cache. Linux native: no cache. | Zero per access |
| An action reaches the cache directory | Container: not mounted. macOS: sandbox profile (C-3b). Linux native: no cache. | Zero |
| A daemon bug leaves a partial or wrong file under a final name | Fills land in `tmp/`, hashed while written. They are closed, chmodded and given the fixed mtime, then renamed with `renameat2(RENAME_NOREPLACE)` (macOS: `renamex_np(RENAME_EXCL)`), under the key's fill-map slot. An indexed entry is never filled again, so a live entry's inode is never replaced under a lease; an unindexed name found first is re-hashed and adopted, or unlinked, before the fill fetches (6.9). An EEXIST at the rename means something outside the daemon made the name: the tmp file is removed and the lease gets a private copy. Nothing is ever opened for writing under `blobs/`. | One rename per insert |
| A crash or power loss mid-insert | Nothing reaches a final name before it is hashed and renamed, and `tmp/` is cleared at start (once the lock is held), so a crash before the rename leaves no entry. A file renamed in after the last snapshot is not in the loaded index: a fill adopts it only after re-hashing it, and the adopt pass adds it as unverified (6.8, 6.9), so it is never served unhashed. A final name whose data never became durable (ext4 delayed allocation) is caught by the size check or by lazy verify. **No fsync per insert**: that would be about 20,000 fsyncs on a cold zig fetch (NativeLink caps its write concurrency because each write syncs). Lazy verify covers the gap. | None per insert |
| Bit rot, or an operator editing the cache, across a restart | **Lazy verify**: after each start, an entry is re-hashed the first time a lease uses it, and only then marked verified in memory. A mismatch evicts it, and the lease fetches over the network (`cache_verify_failures`). The zig tree is hashed once per daemon start, not once per action. | Once per entry per start |
| Accidental mode or owner change | `fstatat` on every hit: owner is the daemon's uid, mode 0444/0555, `st_size` equals the key's size, regular file. A mismatch evicts the entry. **This is a check against accidents, not a defence against a same-uid writer**: that writer can restore every field (5.2b). | One stat per hit |
| Two daemons on one cache directory | An exclusive `flock` on `<cache-dir>/lock`, taken before any startup cleanup or snapshot load, on a file that is never unlinked (6.1): one owner per directory. A second daemon warns, runs uncached (`cache_disabled_reason=locked`) and changes nothing under `<cache-dir>`. Shared use is refused because each process would rebuild its own byte total, neither would count the other's inserts, and each would evict the other's pinned entries. | None |

Rejected: re-hashing on every link (the zig tree's full size per action, the very cost
the cache removes), and trusting entries after a restart without a verify, and wiping the cache at start (6.8).

### 6.4 Concurrency and fills

- **One fill per key.** An in-process map `HashMap<Key, Shared<BoxFuture<Result<(),
  CasError>>>>` behind a mutex. The first miss takes the key's **slot** and starts the
  fill: an `fstatat` of the final name (an unindexed file there is re-hashed and adopted,
  or unlinked, 6.9), then engine `read_to` into `tmp/`, or `read_many` for a small blob,
  then a write. The evictor and the adopt pass take the same slot (6.9). Later misses for the key await
  it, then link. The entry leaves the map when the future resolves. A failed fill fails
  every waiter with the same `CasError`, so `Missing` stays FAILED_PRECONDITION for each
  of them. One process owns the directory (6.3), so no coordination across processes
  is needed.
- **Fills go through the engine** and take its network permits. Hits take only the
  cache's `--cache-link-concurrency` (default 64 on Linux, 5 on macOS), so a hit never
  waits behind a fetch.
- **Cancellation** follows the engine (4.4): a fill with no waiters left is cancelled
  unless more than half its bytes have arrived. Its tmp file is removed.
- **ETXTBSY.** A fill is closed before it is renamed. The remaining race is the one
  BuildBuddy hit: a child forked while a `tmp/` write descriptor is open holds it until
  its exec. If that file is renamed and executed in that window, exec fails with
  ETXTBSY. The native driver already retries a busy program for 2 s (`BUSY_WAIT`). The
  container driver maps a container that did not start to exit 126 (`program.rs`);
  C1 adds one retry of `podman start` when crun reports ETXTBSY (test C-9).

### 6.5 Sizing, eviction and the link limit

- **Budget.** `--cache-max-bytes` sets the budget; unset means no cache, which stays the
  open-source default. CEO decision (2026-10-13): the farm runs the cache as an LRU with
  a **400 GB maximum per node**, set in the farm's deploy config, not in kbf. C1 also has
  `--cache-min-free-bytes`, a floor on the filesystem's free space (`statvfs`). The farm's starting floor is 100 GB (*unmeasured*), tuned from
  `cache_evictions` and `cache_floor_unreachable`.
- **Policy: LRU** by last link time, in memory, persisted through the index (6.9). Buildbarn recommends RANDOM for the worst
  case, but says LRU is needed when inode identity matters, and LRU keeps a hot
  toolchain resident.
- **Eviction unlinks the cache's name only.** A running lease's links keep the inode alive;
  its space is freed when the lease directory is removed.
- **The free-space floor counts only reclaimable bytes.** An entry with `st_nlink > 1`
  (linked by a running lease) frees nothing when evicted. Free-space eviction therefore
  **skips entries with `st_nlink > 1`**, counts only `nlink == 1` entries as
  reclaimable, and stops when none are left, logging `cache_floor_unreachable`.
  Otherwise lease scratch below the floor would empty the whole cache without reaching
  it, and every later lease would be cold. Byte-budget eviction is unchanged: the
  budget counts entries, whoever links them.
- **Pins.** Keys a lease is materializing are pinned until its `materialize` returns. If
  a link still races an eviction, `linkat` returns ENOENT. The daemon then refills that
  key and links again (NativeLink's path). The pin makes the race rare and the retry
  makes it safe.
- **Empty files are created directly**, never linked or looked up (BuildBuddy). The
  empty blob is the commonest repeated digest, and linking it would reach the link
  limit soonest.
- **Link limit.** The daemon reads the limit with `pathconf(_PC_LINK_MAX)` (65,000 on
  ext4). A key whose `st_nlink` is within 1,000 of it is **copied** for the lease rather
  than linked, once per lease per key (6.2). A real EMLINK or EXDEV does the same, and
  each case bumps `cache_link_fallbacks`. Neither fails the lease. (The research grepped
  Buildbarn and found no EMLINK or EXDEV handling; it inferred such an error would
  surface as `Internal`.)

### 6.6 Directory-level caching

The zig tree is one subtree with a stable digest. There are three steps, each taken only
if per-lease metrics show the previous one is not enough:

1. **Directory cache (C4a).** An in-memory LRU of decoded `Directory` messages keyed by
   `(digest, is_tree_root)`, shared by all leases (Buildbarn's key: a Tree's root
   Directory does not hash to its own digest, so the two key spaces must not mix).
   Directory blobs also go into the file cache. A cold walk uses `GetTree` (S1): one
   streamed call, following `next_page_token`, instead of one round trip per level.
2. **Subtree link plans (C4b).** For a Directory with at least N files under it (for
   example 256), the daemon keeps the flattened `(relative path, key)` list after the
   first materialize. A later lease with the same subtree digest skips the walk and the
   name checks and runs the links in a tight loop. The plan comes from messages
   `check_name` already checked.
3. **Whole-subtree materialization.**
   - **macOS (C4c):** keep `v1/dirs/<digest>/`, an immutable materialized copy, and give
     each lease one `clonefile(dirs/<digest>, lease/root/<path>)`. `clonefile` copies
     directory modes too, and the cached directories are read-only. A walk must
     therefore make the cloned directories writable, which takes back part of the
     one-syscall saving. C4c measures what is left.
   - **Linux container:** the equivalent is a second `:O` overlay volume with the cached
     subtree as its lower. A `rename` across that mount boundary fails with EXDEV. It
     is considered only if C4b measures too slow, and is refused whenever an output path
     or the working directory lies under the subtree.

The freshness rule in `daemon.md` ("a marker left by one lease is never seen by the
next") gates every step here: test C-12.

### 6.7 Outputs into the cache (C4d)

A compile's outputs are often the next action's inputs on the same node. An output
moves into the cache only when **all** of these hold:

- the action is dead: container cgroup killed; native kill loop done;
- the output is already uploaded, so its digest is known;
- the file is regular and `st_nlink == 1`, so no other name, and no surviving process
  through another link, can reach it.

How it moves:

- **Container:** the file in `upper` was handed back by `chown 0:0`, which clears
  `security.capability`. The daemon **strips every xattr**, including the `user.overlay.*`
  that overlay writes with `userxattr`, and the POSIX ACL
  (`system.posix_acl_access`). Then it renames the file into `tmp/`, chmods it 0444/0555,
  sets the fixed mtime and inserts it (6.3). No copy is made.
- **Copy mode** (`--cache-dir` on another filesystem than `--scratch`, 6.1): the rename
  from `upper` into `tmp/` would fail with EXDEV, and on macOS `clonefile` would too.
  The output is then **not inserted**; it stays where it is and is removed with the
  lease. Copy mode is a warned misconfiguration, and a copy here would write every
  output twice to save a later miss that the network fill already covers. The same holds
  for an EXDEV on any single output. Skips are counted in `cache_output_skips`.
- **Native (macOS):** a chmod does not revoke a write descriptor still open in a process
  that survived, so the file is never renamed. The daemon `clonefile`s it into a fresh
  inode in `tmp/`, strips the clone's xattrs and ACL (`acl_set_fd` with an empty ACL),
  and inserts the clone.
- Lazy verify still applies after a restart. Outputs never answer FindMissing (contract 1).

### 6.8 Crash and restart

The cache persists across restarts from its first PR, C1 (komira-build-farm#527 is
folded into it). There is no wipe-at-start stage, and the cache never waits on a scan:
daemons restart on every deploy, and neither a cold node nor a node that runs uncached
while it lists 400 GB of files is acceptable. The daemon keeps an **index** on disk, one
snapshot file (6.9), and loads it at start. Start-up, in order:

1. **Take the lock** (6.1). A daemon that cannot take it touches nothing under
   `<cache-dir>` and runs uncached.
2. **Clear `tmp/` and `index.snap.tmp`.** Every file in `tmp/` is a fill that never
   reached its rename, so no partial file is ever under a final name; `index.snap.tmp` is
   a snapshot that never reached its rename. Move an unknown layout version aside, with
   its index.
3. **Load the snapshot** (6.9). It yields every entry's key, size and last use, the LRU
   order and the byte total. A missing, corrupt, truncated or unknown-version snapshot is
   discarded with a warning (`cache_index_discarded=missing|corrupt|version`), and the
   in-memory index starts empty. Start-up never blocks on anything but this one read
   (`cache_index_load_ms`: 339 to 373 ms for the 56 MB snapshot of 1M entries, measured
   in C1; see "C1 as built"). The snapshot
   thread then writes a snapshot at once (6.9), so a well-formed snapshot of what was
   loaded exists before the first lease.
4. **Serve at once, and drain to the limit.** The cache **serves at once**: hits, fills
   and inserts. If the total exceeds `--cache-max-bytes` (the flag was lowered between
   runs), the evictor drains the oldest entries by last use through its normal path
   (6.9): one key at a time, it takes the key's slot, re-checks that the key is still
   indexed and unpinned, removes it from the in-memory index, unlinks the name and
   releases the slot. The in-memory remove never happens outside the slot, so a lease
   that asks for a key mid-drain either hits it (not yet reached; its pin makes the
   drain skip it), waits on the slot and then misses, or misses outright; in each case
   the key's file exists exactly when it is indexed. The total is over the limit only
   until the drain ends.
5. **Re-hash on first use.** Every loaded or adopted entry starts unverified, and is
   re-hashed the first time a lease uses it (lazy verify, 6.3). The index says what should
   be on disk, never that the bytes are right. A mismatch evicts the entry and the lease
   fetches over the network.
6. **Adopt in the background** (6.9): files on disk that the index does not list are
   **added** to it, and entries whose file is gone are dropped. The pass never deletes a
   file and never blocks a lease.

**Disk and index disagree after a crash in two ways, and both are safe:**

- **The index lists a file that is gone** (evicted after the last snapshot, or deleted by
  an operator). The lease's `linkat` returns ENOENT, which is the existing eviction-race
  path (6.5): the daemon drops the entry, refills the key and links again. It is a miss,
  never an error. The adopt pass drops the entries nobody asks for.
- **A file is present that the index does not list** (inserted after the last snapshot,
  or every file after a discarded snapshot). Lookups go through the in-memory index, so
  it is not served as a hit. A fill for that key finds the name (6.4) and adopts the file
  only after re-hashing it, or replaces it; the adopt pass adds the rest as unverified.
  No crash state serves a byte that was not hashed in this run.

**What the byte total can miss, and for how long.** The total is the in-memory index's,
and the size limit is enforced against it. A snapshot is written right after load, every
5 minutes, at a clean stop, and right after each adopt pass completes (6.9). One pass
takes 256 directories at one per second plus the listing time: about 5 minutes,
*unmeasured* at 1M entries (C1 defers it; see "C1 as built"). The bytes on disk that
the total does not count are, at any moment, at most:

- **Fills since the last snapshot that was written, across runs.** After one crash that
  is at most 5 minutes of fills plus one snapshot write, counted when the next pass
  ends. It is not bounded by one interval in two cases. In a **crash loop** where no run
  lives through a whole pass, every run loads the same old snapshot and the unindexed
  fills of all those runs add up until one run completes a pass. While **snapshot writes
  fail** (ENOSPC), the window grows for as long as they fail; the in-memory total of the
  running daemon stays right, and only a crash in that time widens what the next run
  misses.
- **Keys a pass skipped.** The pass skips a key whose slot a fill holds. A fill can end
  without indexing the name it found or made (cancelled while re-hashing an unindexed
  name, 6.4; an EEXIST that hands the lease a private copy, 6.9). Such a fill records the
  key, and the next pass visits recorded keys first: those bytes are uncounted for at
  most one pass interval (a day).
- **After a discarded snapshot:** the whole previous cache, up to 400 GB, until the first
  pass completes. While it runs, the disk holds the old cache plus new fills of keys it
  does not hold (a fill for a key already on disk adopts the file and fetches nothing):
  about twice the limit at worst.

**The disk-size assumption.** `--cache-dir` shares a filesystem with `--scratch` (6.1), so
a full disk fails a lease's own copy and its outputs too. The cache never fails a lease
that would have run without it only if the filesystem holds **twice `--cache-max-bytes`
plus the scratch headroom (`--cache-min-free-bytes`)**. On a smaller disk, leases can fail
with ENOSPC during the window after a discarded snapshot or a crash loop; the farm's
deploy config must meet it (*unmeasured* for the farm's nodes). To shorten that window,
**free-space pressure speeds the pass**: while `statvfs` is below the floor and the floor
cannot be reached by evicting indexed entries (`cache_floor_unreachable`), the pass drops
its one-directory-per-second rate limit, so uncounted bytes become indexed, and so
evictable, as fast as the disk can list them. Once the pass has adopted everything, LRU
eviction brings the total under the limit.

The other direction (counted bytes that are gone) runs the cache under its budget, and
ENOENT or the pass corrects it.

Leases that outlive the daemon are safe: eviction removes only the cache's name, and a
re-adopted lease's links keep its inodes alive (`daemon.md` § Planned, re-adoption).
Free-space eviction `fstatat`s each candidate and skips `st_nlink > 1` (6.5).

- **Disk full mid-fill.** The write to `tmp/` fails and the tmp file is removed. The lease
  falls back to writing its own copy (today's path), and eviction is kicked. Given the
  disk-size assumption above, ENOSPC in the cache never fails a lease that today's code
  would run. ENOSPC on a snapshot write
  removes `index.snap.tmp`, keeps the previous snapshot, logs, and retries at the next
  interval.
- **Cache unusable** (missing, read-only, unknown layout, locked, wrong graph driver,
  native Linux): the daemon warns, reports `cache_disabled_reason`, and runs uncached.
  A bad snapshot is not "unusable": it costs LRU order for one pass, never the cache.

### 6.9 The index

**One file.** `v1/index.snap`, beside `blobs/` and `tmp/`. It sits inside `v1/` rather
than beside the lock because it describes `v1/`'s blobs: moving an unknown layout aside
takes its index with it. Only the daemon's snapshot thread writes it.

**Format, our own code, pure Rust.** A 16-byte header: magic `KBFCIDX\0`, format version
(u32, 1), 4 bytes reserved. Then the entry count (u64) and one 56-byte entry per cache
entry, in key order, little-endian: flags (u8, bit 0 = executable), 7 reserved, SHA-256 (32), size
(u64), last use in Unix seconds (u64). Then a CRC-32 (the `crc32fast` crate) over every
byte before it. There is no per-entry checksum, no log, no generation and no compaction:
the file is always written whole.

**When it is written.** Right after load (6.8 step 3), every 5 minutes, right after each
adopt pass completes, and on a clean stop, from the in-memory index, on the snapshot
thread. The lease path never does index I/O: a hit updates the entry's last
use in memory (from the daemon's clock, which tests replace with a fake one), and nothing
else. The thread copies the entries under the index lock (no I/O under it; at 1M entries the
longest a caller waited on the lock was 23 to 24 ms, measured in C1), then, without the
lock, writes
`index.snap.tmp`, `fsync`s it, renames it over `index.snap` and `fsync`s `v1/`. A crash at
any point leaves the previous snapshot or the new one, never a mix; the temp file is
cleared at the next start. At 1M entries that is 56 MiB written per 5 minutes, about
0.2 MB/s on the NVMe disk.

**Loading.** One sequential read. The snapshot is discarded, and the in-memory index
starts empty with a warning, if the magic or version is unknown, if the length is not
16 (header) + 8 (count) + count x 56 + 4 (CRC) bytes, if the CRC does not match, or if a key appears twice. A
discarded snapshot never blocks start and never discards a file: the adopt pass and the
fills take the files back. Loaded entries are unverified (6.8 step 5).

**Each key's slot in the fill map (6.4) serialises everything that changes that key's
name**, so the index and the directory agree for that key whenever the slot is free:

- **A fill** takes the slot on an index miss. It `fstatat`s the final name first. If a
  file is there (inserted after the last snapshot, so not indexed), it is stat-checked
  (6.3) and re-hashed: a match is adopted as a verified entry and nothing is fetched; a
  mismatch is unlinked. Only then does the fill fetch into `tmp/` and
  `RENAME_NOREPLACE` it into `blobs/`. With the slot held, nothing in the daemon can
  create that name meanwhile, so an EEXIST means something outside the daemon did: the
  fill removes its tmp file, logs, and the lease gets a private copy.
- **The evictor** (budget, free-space floor, the at-load drain of 6.8 step 4, a failed
  stat check or verify) takes the slot, re-checks that the key is still indexed and unpinned, removes
  it from the in-memory index, unlinks the name, and releases the slot. A key whose slot
  a fill holds is skipped for that round.
- **The adopt pass** (below) takes the slot for each key it changes.

**The adopt pass.** One background task, rate-limited to one `blobs/<h0h1>/` directory
per second (lifted under free-space pressure, 6.8), one pass after start and then one a
day; keys a fill recorded as left unindexed (6.8) are visited first. For each directory it lists the
names and compares them with the index's entries for that prefix. Then, per key it would
change, it takes the slot; if a fill holds it, it skips the key (the fill will index it);
and with the slot held it **re-checks the index**, because the listing is already stale:

- **A file the index does not list:** `fstatat` it; if it is a regular file, it is added
  with size from `st_size`, last use from `st_ctime` (set by the rename and by each
  `linkat`, so close to the last link), and **unverified**, so its first use re-hashes it.
  The total grows by its size, and eviction applies to it like any other entry.
- **An entry with no listed file:** `fstatat` the name; only if it returns ENOENT is the
  entry dropped. A file renamed in after the listing is kept.
- A name that does not parse as a key, or a file that is not regular, is left alone,
  logged and counted (`cache_foreign_files`); its bytes are never in the total. The pass
  unlinks nothing.

It holds no mutex across a system call; a fill that wants a key the pass holds waits for
one `fstatat`. It never waits on a lease. Counted in `cache_adopted_entries`,
`cache_dropped_entries` and `cache_index_drift_bytes` (the bytes the last completed pass
added plus the bytes it dropped).

**Rejected for the index:**

- **An append log over the snapshot** (revision 4: per-record checksums, generations,
  compaction, a deleting sweep). A review of it found three defects that follow from the
  log itself: a fresh or discarded index stayed cold until the first compaction, because
  a log was used only under a snapshot of its own generation; a short write in mid-log
  lost every later record at the next load, and a dropped batch could double-count on
  replay; and the bound on unindexed files was false, since dropped batches are
  unbounded and a deleting sweep turns a lost index into 400 GB to delete and refetch.
  Adopting files makes a lost record cost only LRU order, so the log buys nothing.
- **redb** (pure Rust, crash-safe B-tree): a second file format and recovery procedure to
  trust, and after an unclean shutdown it repairs its allocator state on open, a cost on
  exactly the restart this design keeps instant (*unmeasured*). **SQLite**: C, so not
  pure Rust. **sled**: pre-1.0, with its own recovery.
- **Per-blob xattrs or sidecar files**: reading them back is a scan before serving.
- **Deleting unindexed files**: see the append log above.

---

## 7. Cache metrics, through the API

Per lease, `kbf.worker.v1.InputStats` goes into `auxiliary_metadata`, next to
`TransferStats` (4.6): `files`, `directories`, `empty_files`, `bytes_total`,
`cache_hits`, `cache_hit_bytes`, `cache_misses`, `fill_waits` (a waiter on another
lease's fill), `link_fallbacks`, `verify_hashes` (lazy-verify work this lease paid
for), `walk_ms`, `link_ms`, and `link_mode` (`hardlink`, `clonefile`, `copy`, or
`off:<reason>`). Network bytes are `TransferStats.read_bytes`, not counted a second
time here.

Per node, on the operator API: `cache_bytes`, `cache_entries`, `cache_evictions`,
`cache_verify_failures`, `cache_pinned_bytes`, `cache_reclaimable_bytes`,
`fs_free_bytes`, `cache_floor_unreachable`, `cache_output_skips`, `cache_disabled_reason`,
`cache_index_load_ms`, `cache_index_discarded`, `cache_adopted_entries`, `cache_dropped_entries`,
`cache_foreign_files`, `cache_index_drift_bytes`.

The cache's acceptance check is one of these numbers: **a second lease with the same zig
tree on the same node fetches 0 bytes for the toolchain** and has
`cache_hits == files - empty_files`. C1 checks this with a test counter and a log
line. C3 puts it in the API.

---

## 8. What placement learns

The cold cost of the zig tree is **once per node**. With a handful of nodes,
locality-aware placement saves at most (nodes - 1) cold fetches of one tree, which is
noise once the cache exists. So placement changes **last**, and only if metrics say so:

- **C5a, report only.** A new `Heartbeat` field, `held_trees` (at most 32 entries), lists
  the directory digests with the most cached bytes behind them, plus a `cache_bytes`
  total. The hot set stays out of the Hello's capabilities: those are hashed into
  `report_hash`, which would churn on every insert. The server stores it per worker and
  serves it on the operator API. It has no scheduling effect.
- **C5b, tie-break only, if justified.** Among feasible workers with room, the scheduler
  would prefer one that holds the request's largest input subtree. The scheduler today
  knows only the action digest, so the front would have to read the input root's
  Directories at Execute. That is new server work, and it would never be a feasibility
  constraint (Q2).

---

## 9. Rollout

| Stage | PR content | Depends on |
|---|---|---|
| **A** (`daemon/parallel-cas`, in progress) | Concurrent fetch in `materialize` and concurrent output uploads, each distinct digest once, with per-call limits (section 1) | nothing |
| **T1, the transfer engine** | `kbf-transfer` core and REAPI backend; one gRPC HTTP/2 connection with many streams; one in-flight limit and byte budget for the process; ByteStream reads (memory and streamed-to-file, resumable) and chunked writes; FindMissing with an UNIMPLEMENTED fallback; window and decode limits; `TransferStats` through the invocations API; the flags of 4.5. Removes stage A's per-call constants. Tests C-11, T-1, T-2, T-3. | A |
| **S1, server** | `WorkerCas` on the worker listener: FindMissing, BatchRead, BatchUpdate, GetTree, each admitted exactly as `WorkerBlobs::admit` admits ByteStream (one admission rule). `batch_read_blobs` reads a batch's blobs concurrently (bounded, e.g. 32) and coalesces adjacent records in one segment into one range GET. Helps buck2's own batch reads with no client change. Test C-15. | nothing; in parallel with T1 |
| **P, plain text** | Lifts the three plain-text refusals of 4.7 | nothing |
| **C1, the first cache** (folds in komira-build-farm#527) | Container driver only, persistent from the start. `kbf-daemon::cache`: layout; the exclusive lock, taken before any startup cleanup, on a file never unlinked; `tmp/` cleared at start; fills hashed into `tmp/` and inserted with `RENAME_NOREPLACE`; the index (6.9: one snapshot file written after load, every 5 minutes, after each adopt pass and at a clean stop, discarded if malformed) loaded at start, so the cache serves at once while the evictor drains to the limit (6.8); the fill-map slot taken by fills, the evictor and the adopt pass; the rate-limited adopt pass, which deletes nothing and speeds up under free-space pressure; lazy re-hash on first use after start; the stat check; one fill per key; LRU by bytes; pins; the ENOENT retry; empty files created directly; link-limit copy; one inode per key per lease; the free-space floor that skips `nlink > 1`. `chown-dirs`. `prepare_outputs` copies instead of renaming. The ETXTBSY retry. Flags `--cache-dir`, `--cache-max-bytes`, `--cache-min-free-bytes`, `--cache-link-concurrency`. Tests C-1, C-2, C-4, C-7 to C-10, C-12, C-13, C-16 to C-20, R-1 to R-9. | T1 planned first; correct without A or T1 (see below) |
| **T2, batching (fix 3)** | `read_many` and `upload_missing` batch small blobs (4.3); per-blob status; the limit from GetCapabilities, or `--transfer-batch-bytes` alone where it answers UNIMPLEMENTED (4.3). Test C-14. | T1, S1 deployed |
| **C3, cache metrics** | `InputStats` and per-node cache metrics through the APIs (section 7) | C1, T1 |
| **C2, macOS** | Decided in [macos-end-state.md](macos-end-state.md) (step 5): hard links once actions run as pool lease users, not the daemon's user. A root-owned read-only cache on a dedicated APFS volume, daemon-made input directories owned by the slot uid, `clonefile` private copies for inputs an output names, behind `--cache-dir`. Until lease users exist: `clonefile`; the sandbox check. Tests C-3, C-3b and that document's step 5 tests; Spotlight and backup exclusion; link-concurrency measurement | C1; lease users (macos-end-state.md steps 3, 4) |
| **C4a-d** | Directory cache and GetTree; subtree link plans; macOS directory clone; outputs into the cache, including the copy-mode skip. Tests C-12 (C4b, C4c), C-21. | C1; GetTree needs S1 |
| **C5a(-b)** | `held_trees` heartbeat report; a placement tie-break only if justified | C3 |
| **T3** | The server's object-store traffic moves onto the engine core | T1, S1 |
| Linux native cache | Not planned: it waits for actions to run under a uid other than the cache owner's (5.2b) | a uid split |

**What C1 leaves out**, to stay as small as persistence allows: the macOS driver (C2),
the cache metrics in the API (C3; C1 has a test counter and a log line), directory-level
caching and outputs into the cache (C4a-d), placement (C5), and any Linux native cache.
Fills use T1's `read_to`/`read_many`; batching (T2) changes nothing in C1.

**What the cache depends on.** C1 needs neither A nor T1 to be correct: it takes
`&impl Cas`, and it rebases onto whichever `materialize` is on `main` (A changes the
walk's concurrency; C1 changes what each file step does). The plan orders A, then T1,
then C1, for the reasons below; it is an order, not a coupling.

**Why the engine goes right after A and before C1, and not merged with fix 3.**

- **Before C1.** Cache fills are network fetches. Built on A's per-call limits, C1
  would leave 16 leases of misses as 16 independent pools on one connection, and its
  fill path would have to be moved onto the engine later: a second migration of the
  same code. With T1 first, C1 calls `read_to`/`read_many` and is done.
- **Not merged with fix 3.** Batching is the most intricate part: per-blob status, size
  discovery, permit accounting, linger. Its gain depends on S1's parallel server read,
  and the research gives no client precedent that it pays (Buildbarn never batches).
  Tying it to T1 would put an unmeasured gain and a server dependency on the critical
  path to C1. T1's API takes sets from day one, so turning batching on in T2 changes
  no caller.
- **Reversible.** C1 takes `&impl Cas`, so it is correct over any `Cas` and does not
  need A or T1 to work. If either slips, C1 can land first.

Every new module stays under 1,000 lines; `cache` is split into
`cache/{layout,fill,evict,verify}.rs` from the start.

---

## 10. Test plan

Every test names the defect it catches and the mutant that makes it go red. C-1, C-2,
C-9, C-12 and C-18 need real rootless Podman (the M2 harness, `kbf-it`). C-3 and C-3b
need macOS. The rest run against `MemoryCas`, `kbf_front::routes` in process (which
serve FindMissing and Batch today), or a counting fake CAS.

| # | Test | Defect caught | Mutant |
|---|---|---|---|
| C-1 | Real Podman: an action tries `echo x >> in`, `truncate -s0 in`, `chmod u+w in`, `open(O_RDWR)` and `ln in in2; echo >> in2` on a cached input. A second lease reads `in`, and the entry is re-hashed. The test records the graph driver and mount options. | A cached inode that a container can write or re-own | `chown -hR 1:1` over `root`, files included |
| C-2 | Real Podman: an output path names an input file, and the action appends to it. The output is right and the cache entry is unchanged. | A linked input moved under `upper`, then re-owned by the upper chown | Keep `prepare_outputs`' rename, with the upper chown in place |
| C-3 | macOS: an action appends to, chmods and `chflags uchg`s a cloned input. The entry is unchanged, and the lease directory is still removed. | A hard link on the native driver | `linkat` instead of `clonefile` |
| C-3b | macOS: a sandboxed action tries to `chmod u+w`, write, `chmod a-w` and `utimes` a cache entry **by its cache path**. Every step is denied, and the next lease gets the right bytes. | A sandbox profile that lets actions reach `--cache-dir` | Allow writes to `--cache-dir` in the profile |
| C-4 | 32 concurrent leases over one tree with a counting CAS: each blob is fetched exactly once. | Fills not merged per key | Remove the fill map |
| C-7 | An entry chmodded to 0644 is evicted at its next use. | No stat check on a hit | Skip the `fstatat` |
| C-8 | The budget is exceeded: LRU entries are evicted, pinned keys survive, a running lease's linked file stays readable after its cache name goes, and the byte total equals the on-disk sum. | Evicting pinned keys; wrong accounting | Ignore pins; count `st_blocks` twice |
| C-9 | An executable inserted while the daemon spawns children in a loop: no lease fails with 126/ETXTBSY beyond one retry. | The ETXTBSY race | Remove the retry |
| C-10 | One digest appears both executable and not in one tree: two entries, modes 0555 and 0444, each linked to the right names. | A key without the exec bit | Drop `.x` from the key |
| C-11 | (T1) An output present locally but missing on the server is uploaded. One the server holds is not (`outputs_present`). Against today's worker listener, FindMissing is UNIMPLEMENTED: everything is uploaded, the call is logged once, and no lease fails. | The cache answering FindMissing; no fallback | Short-circuit uploads on a local hit; treat UNIMPLEMENTED as fatal |
| C-12 | Freshness: lease 1 creates, deletes and renames files in a cached subtree. Lease 2 with the same digest sees the pristine tree (C1, C4b, C4c). | One lease's changes leaking to the next | Reuse a lease's materialized subtree as the cached directory |
| C-13 | `--cache-dir` on another filesystem than `--scratch`: a warning, `link_mode=copy`, leases succeed. EMLINK is simulated through a fake link operation. | No EXDEV/EMLINK fallback | Remove the fallback |
| C-14 | (T2) A 3,000-blob tree is fetched in batches of at most 2 MiB. A full 4 MiB server batch decodes. A per-blob NOT_FOUND gives `Missing`. A corrupt blob in a batch is refused. One batch holds one permit. | tonic's 4 MiB default; per-blob status ignored; unhashed batch bytes; a batch taking N permits | Default decode limit; skip `digest_of`; one permit per blob |
| C-15 | (S1) A 100-blob `BatchReadBlobs` against an object store with 10 ms per GET finishes well under 100 x 10 ms. | Serial batch reads | Revert to the serial loop |
| C-16 | A second daemon on a locked `--cache-dir` warns, reports `cache_disabled_reason=locked`, runs uncached, and its leases succeed. Daemon 1 holds a fill open in `tmp/` (the R-1 fault hook) when daemon 2 starts. The first daemon's entries, its lock file (same inode) and its `tmp/` fill are unchanged, the fill then completes and inserts, and a third daemon is still refused. | Two owners sharing one cache's accounting; startup cleanup that runs before the lock and deletes the owner's fills or lock file | Take the lock shared; clear `tmp/` before taking the lock; unlink and recreate the lock file at start |
| C-17 | Zig-shaped tree (20,000 files, above the 15,189 of the zig toolchain komira pins, section 1): lease 2 fetches 0 bytes and `cache_hits == files - empty_files`. `link_ms` is reported, not gated. | The cache not consulted on the hot path | `materialize` ignoring the cache |
| C-18 | Determinism: a tree with two identical-content files `a` and `b`, run twice, once with EMLINK forced on that key. `stat` of `a` and `b` inside the action shows one inode both times, and a `tar` of the inputs is identical across runs. | A fallback that gives some names a private copy and others the cache inode | Fall back per name instead of per key per lease |
| C-19 | Linux native with `--cache-dir`: a warning, `link_mode=off:native-same-uid`, and the cache directory is never created or read. | Enabling a cache that a same-uid action can poison | Enable the cache on Linux native |
| C-20 | Free-space floor: leases hold links to most entries, and scratch pushes free space below the floor. Only `nlink == 1` entries are evicted, `cache_floor_unreachable` is set, and the linked entries survive. | A floor that empties the cache without freeing space | Ignore `st_nlink` in free-space eviction |
| C-21 | (C4d) Outputs into the cache: an output with a second hard link, an output carrying a `user.*` xattr and an ACL, and (macOS) an output with a write descriptor held by a surviving child. The first is not inserted. The second is inserted without xattrs or ACL. The third's entry is a fresh inode, unchanged after the child writes. In copy mode no output is inserted, `cache_output_skips` counts them, and the lease succeeds. | An action's inode or its metadata entering the cache; an EXDEV failing the lease | Skip the nlink check; skip the strip; rename instead of clone on native; treat EXDEV as an error |
| T-1 | (T1) 16 leases of 1,000 blobs each against a counting server: concurrent requests never exceed `--transfer-in-flight`, and each digest is fetched once. All of it runs over one connection. | Per-call limits multiplying; duplicate fetches | Restore a per-call semaphore; remove the in-flight map |
| T-2 | (T1) A connection reset mid-stream: every in-flight stream fails, the engine reconnects, each read resumes from its offset, every blob hashes right, and `retries` equals the number of streams that were in flight. | No retry; a resume that duplicates or skips bytes | Resume at offset 0 while appending |
| T-3 | (T1) A lease dropped with queued requests: they never start, and a shared fetch with another waiter completes. | Leaked or wrongly cancelled work | Cancel shared fetches on drop |
| R-1 | (#527) A fill is killed between its last write and the rename (fault hook), and the daemon is restarted. No entry exists under the final name, `tmp/` is empty, and the next lease fetches the blob and gets the right bytes. | A partial file under a final name; `tmp/` not cleared | Fill straight into `blobs/`; skip the `tmp/` clear |
| R-2 | (#527) Two daemon runs on one `--cache-dir` with a counting CAS: run 1 fills the zig-shaped tree; after a clean stop and restart, with the adopt pass paused by a test hook, the **first** lease for the same tree reads 0 blob bytes from the counting CAS, every file is a hit from the loaded index (C1's test counter of fills is 0; a fill that adopts an existing name would also fetch nothing, so bytes alone cannot tell), and each entry it uses is re-hashed exactly once (the verify counter equals `files - empty_files`; `InputStats` reaches the API only in C3). | A cache that does not survive a restart, a snapshot that is not written at a clean stop, or one that serves only after a scan | Wipe `v1/` at start; skip the snapshot at a clean stop; ignore the snapshot and start empty |
| R-3 | (#527) While the daemon is stopped, one entry has a byte flipped (same size, mode and mtime restored). After restart its first use re-hashes, evicts and refetches it; `cache_verify_failures == 1`; the lease gets the right bytes. | Serving an entry altered while stopped | Mark every entry verified at index load |
| R-4 | (#527) Run 1, with a fake clock, inserts keys K1..K6 and then uses them in the order K3, K1, K5, K2, K6, K4, one fake minute apart; a clean stop writes the snapshot. Run 2 starts with `--cache-max-bytes` lowered to hold three entries. With no lease, the drain removes exactly K3, K1, K5 (the least recently used in run 1's real order, not in insert order), and the loaded last-use values equal run 1's; then the total is at or under the limit and equals the on-disk sum. Run 2b repeats run 2 with the drain held by a test hook inside K1's slot (after the in-memory remove, before the unlink), K3 already drained and K5 not yet reached, while one lease needs K3, K1 and K5: K3 is a miss and is refetched; K1's fill waits on the slot, re-checks the index and refetches; K5 is a hit. After the hook is released, every indexed key's file exists, each key is indexed once, and the total equals the on-disk sum. | The limit enforced only on insert; recency lost across a restart; a drain that removes keys from the in-memory index outside their slots (then a fill adopts a file the drain unlinks) | Skip the at-load eviction; write insert time instead of last use into the snapshot; update last use only in the snapshot thread; remove every drained key from the in-memory index first, then unlink each under its slot |
| R-5 | Crash between snapshot writes: run 1 fills set A, a test hook writes a snapshot, run 1 fills set B, and the daemon is killed (SIGKILL) before the next snapshot. One B file has a byte flipped (same size and mode). Run 2, with the adopt pass paused: a lease needing a good B key reads 0 bytes from the counting CAS and the verify counter rises by one for it (adopted after re-hash); a lease needing the flipped key re-hashes it, fetches it once and gets the right bytes. The pass is then resumed: every other B file is adopted unverified, its first use re-hashes it, no B file was unlinked (inode set unchanged), and the total equals the on-disk sum. | An unindexed file served without a re-hash; files inserted since the snapshot deleted or never counted | Adopt as verified; on EEXIST or an existing name, link it without hashing; make the pass unlink unknown files; adopt without adding the size |
| R-6 | Corrupt snapshot: one byte of one entry's last use is flipped (no per-entry checksum exists, so only the whole-file CRC can catch it); in another run the version is 2, with the CRC recomputed so the version check is what refuses it. The daemon starts without waiting, warns `cache_index_discarded=corrupt` or `version`, and starts with a total of 0. With the pass paused, a lease's keys are adopted by their fills (0 bytes fetched). After one pass every file is indexed, nothing was unlinked, and the total equals the on-disk sum; with `--cache-max-bytes` below that sum, LRU eviction then brings it under the limit. | Trusting a snapshot that fails its CRC; a bad snapshot blocking start, disabling the cache, or costing the bytes on disk | Skip the CRC check; refuse to start on a bad snapshot; disable the cache when it is discarded; make the pass unlink unknown files |
| R-7 | Malformed snapshot: (a) the file is cut inside the header (another run: inside an entry, just before the CRC; another: one extra byte appended); (b) a forged file with a valid, recomputed CRC whose count says one more entry than its length holds; (c) a forged file with a valid, recomputed CRC that lists one key twice. Each run discards it with `cache_index_discarded=corrupt`, starts without a panic and with a total of 0, and the pass adopts every file once. (b) and (c) pass the CRC, so only the length and duplicate checks can refuse them. | A short or padded file read as entries; a short read that crashes start; a duplicate key counted twice | Read entries until EOF without the length check; unwrap the short read; skip the duplicate-key check |
| R-8 | While the daemon is stopped, two indexed blob files are deleted. After restart, a lease needing one gets ENOENT on link, refetches and succeeds with no lease error; one pass drops the other entry, and the total equals the on-disk sum. | An index-listed missing file failing a lease; drift never corrected | Map ENOENT at link to a lease error; make the pass only adopt |
| R-9 | Races on the slot, each held open by a test hook. (a) The evictor is held between the in-memory remove and the unlink of K while a lease needs K: the fill waits on the slot, then fetches and inserts, and afterwards K's file exists and is indexed once. (b) The pass lists a directory, then a fill renames and indexes K in it, then the pass compares: K is kept. (c) The pass lists K's directory and compares (K is not indexed), and a hook holds it between that compare and its slot take; a fill for K then renames, indexes K and releases the slot; the hook is released and the pass takes the slot: it re-checks, finds K indexed and leaves it, and K is counted once. In each, the total equals the on-disk sum. | An index entry with no file (evictor without the slot); an entry dropped on a stale listing; a file counted twice | Evict without the slot; drop an entry without the `fstatat`; adopt without re-checking the index under the slot |

Wall-clock speedups are reported, never asserted.

**Implementation notes for C1** (review findings left to the code, one line each):

- The slot records its holder's kind (fill, evictor, pass); a waiter on a non-fill holder re-checks the index and starts its own fill, and the pass skips only a fill holder.
- Concurrent first uses of one unverified entry share one re-hash, merged per key like a fill.
- macOS (C2): `clonefile` does not change the source's ctime, so the pass's last use there is the insert time; on Linux removing a lease directory also bumps ctime. Both cost LRU order only.
- Add tests for a daemon killed mid-snapshot-write (mutant: write `index.snap` in place), for ENOSPC on a snapshot write (the previous snapshot is kept), and for an outside creator between a fill's `fstatat` and its rename (the EEXIST half of R-5's mutant).

**C1 as built** (where the code differs from the sections above, or settles what they
left open):

- Fills read a blob whole into memory. Before T1 that was within the lease's
  `tree::FETCH_BYTES_IN_FLIGHT`; since T1 it is the engine's `get`, within its byte
  budget for the process ("T1 as built"). `--cache-link-concurrency` bounds a lease's
  keys in flight, hits and fills alike; the engine's permits bound the network.
- A fill is driven by the lease that took the key's slot. If that lease is dropped, the
  fill stops with it and the slot is released; a waiter then takes the slot and fills
  the key itself. There is no "more than half its bytes" rule before T1.
- The slot records no holder kind. Whoever held it, a waiter looks at the index again
  once it is released, and gets a failed fill's error; the adopt pass skips a key whose
  slot anyone holds (the evictor's hold ends with index and name agreeing too).
- `cache_hits` and `cache_misses` count files (names); `verify_hashes` counts entries.
  A key the index did not list but whose file a fill adopted after a re-hash is a miss
  that fetched nothing. C1's counters are `InputStats`, logged once per lease
  (`inputs: files=...`), and `CacheStats` for the node.
- An input an output names is a private copy made by `materialize`; `prepare_outputs`
  still renames it into `upper`. Without `--cache-dir` the driver chowns the input root
  with `chown -hR` as before, so nothing changes for an uncached node.
- The ETXTBSY retry (one more `podman start`) applies with or without the cache. C-9 is
  tested against the stand-in Podman, which reports crun's ETXTBSY: the race itself
  cannot be timed from a test. C-12 has C1's half only (no subtree is reused yet).
- `--cache-dir` must exist; a directory named `kbf-lease-*` or `lease-*` is refused
  (`cache_disabled_reason=lease-name`), not only warned about. Another layout version
  `v<n>` is renamed `v<n>.moved-<unix seconds>`. Podman's storage must be the kernel's
  `overlay` with no `mount_program` (fuse-overlayfs is refused as `graph-driver`).
- A clean stop is the last handle of the cache being dropped: the daemon's exit saves
  the index. Snapshot writers are serialised by a lock of their own.
- **A cached input cannot be renamed or hard-linked in the container**, only read or
  unlinked (C-1 and C-12 on the hosted runners' kernel overlay): overlayfs copies a lower
  file up before a rename or a link, and refuses to copy up one whose owner the caller's
  user namespace does not map (`EOVERFLOW`). Section 5.1 expected the rename to work. An
  input an output names is a private copy, so moving it into its output path works; an
  action that renames another input fails. Decided (CEO): accept it, as Q1 accepted
  `EACCES`. An action that renames some other cached input into an output fails; Q1's
  private-copy opt-out (`kbf-writable-inputs=1`) is added only if a real build breaks.
- Podman's storage is checked before the cache is opened: `CacheUse` carries the cache's
  configuration, and `PodmanRuntime::new` opens it only on storage that passed. On a
  refused graph driver the directory is untouched: no lock, no `tmp/` cleared, no drain,
  no snapshot.
- **Measured at 1M entries** (`crates/kbf-daemon/tests/cache/measure.rs`, an ignored test,
  so off the gate; a release build on one developer machine, an i7-14700K with an NVMe
  disk, three runs): `cache_index_load_ms` 339 to 373 ms for the 56 MB snapshot; the
  longest wait of a call taking the index lock while snapshots copy the entries, 23 to
  24 ms; one snapshot written whole (copy, write, `fsync`, rename), 71 to 189 ms. Not
  measured: the adopt pass at 1M entries (6.8 says about 5 minutes; its 256 paced
  directories alone take 256 s at the 1 s pace, and the listing time needs 1M real files
  on a node's disk, so it is left for the first farm node that holds them), and the
  farm nodes' disks against the disk-size assumption (6.8), a deploy-config matter.
- The toolchain count of section 1 is the archive's (15,189 files): C1 has no farm
  build of zig to log `inputs: files=...` from, so the count per input root waits for
  the first cached farm node.

**T1 as built** (crate `kbf-transfer`; where the code differs from section 4):

- The `Cas` trait moved into `kbf-transfer` (`kbf_daemon::cas` re-exports it), and the
  engine is a `Cas`: `Transfers::new(endpoint, config)` (explicit windows, one
  `connect_lazy` channel), `Transfers::over(channel, config)` for a connection already
  made, `scope(&lease)` and `stats()`. `CasClient` is gone; its calls are the engine's
  backend (`wire`). The trait gained `type Lease` and `lease(&Arc<Self>, &LeaseId)`, so
  each driver takes one scope per lease without naming the engine (a store with
  nothing per lease returns itself), plus `read_to` and `in_flight`.
- **No `read_many` or `upload_missing` method yet.** The callers (`materialize`, the
  cache's fills, the output walks of both drivers) stay generic over `Cas` and make
  per-blob calls, concurrently, with at most `Cas::in_flight` of their own in progress:
  a bound on what one walk holds (open files, fetched bytes), read from the engine's
  one `--transfer-in-flight`, not a second constant. Contract 5 holds: no caller waits
  for one blob before asking for the next. The engine merges reads per digest, and
  gathers the FindMissing questions of concurrent uploads into calls of at most
  `--transfer-find-missing-chunk` digests, one call at a time (the questions that
  arrive during a call wait for the next; no linger). The set methods come with T2,
  where grouping decides the batches.
- `materialize` writes each distinct blob's first file with `read_to` and copies its
  other files from it. `read_to` of a blob up to `--transfer-memory-blob-bytes` goes
  through the shared in-memory fetch; a larger one is streamed into the file and
  resumed from its offset. The byte budget covers a read into memory from its start to
  its answer; bytes a caller holds after that (a fill until its insert) are bounded by
  the caller's own `in_flight` items. Cache fills read through `get`, into memory:
  streaming a fill into `tmp/` is left for later, since an insert that fails hands the
  bytes to the lease to write.
- A write from memory is retried from its first byte; one from a file's chunks
  (`put_chunks`) is not retried, its chunks being read once. Retried codes: UNAVAILABLE,
  and UNKNOWN, INTERNAL, CANCELLED and ABORTED (how tonic reports a stream whose
  connection broke). The backoff is 100 ms, doubling, at most 3.2 s, not a flag. tonic's
  lazy channel connects again on the next request.
- A FindMissing that fails after its retries, or that the server refuses, answers
  "missing": the blob is uploaded. The first UNIMPLEMENTED turns it off for the life of
  the process, with one warning; `EngineStats::find_missing_unavailable` counts it.
- A shared fetch counts its waiters. When the last leaves, the fetch gives up unless
  more than half of the blob has arrived; giving up drops the read, so one only queued
  never reaches the server. Only the fetch's own task takes it out of the map, under
  the map's lock, so no caller joins a fetch that gave up. A fetch that finishes with no
  waiters keeps nothing: its bytes go only to a caller that joined while it ran, not to
  a later lease or the cache (4.4's "for the next lease or the cache" is not built).
- The byte budget is held while a blob is read into memory and released when it lands.
  Each waiter of a shared fetch then gets its own copy (`get` returns a `Vec<u8>`; all
  but the last are clones), outside the budget: 16 leases sharing one fetch hold 16
  copies of its blob until they write them out.
- **`TransferStats` is a log line, not yet the API.** Each lease's scope logs
  `transfers: read_calls=... read_bytes=... merged_reads=... find_missing_digests=...
  outputs_present=... write_calls=... written_bytes=... written_blobs=...
  retries=...` (INFO, field `lease`) once the lease's scope and the fetches it started are gone. FindMissing calls
  are shared by leases, so a lease counts the digests it asked about
  (`find_missing_digests`), not calls. The batch counters wait for T2;
  `permit_wait_ms` and `transfer_ms` are not counted yet. Carrying them in
  `auxiliary_metadata`, the invocation metrics and the operator API (`EngineStats`, a
  test counter for now) is a later stage.
- Flags: `--transfer-in-flight`, `--transfer-bytes-in-flight`,
  `--transfer-memory-blob-bytes`, `--transfer-chunk-bytes`,
  `--transfer-find-missing-chunk`, `--transfer-stream-window-bytes` and
  `--transfer-retries`, with 4.5's defaults, still *unmeasured* (no farm measurement
  ran with this change). `--transfer-lease-in-flight` is not added until a measurement
  shows a need; the batch flags come with T2.
- Tests: T-1 (for reads, and again for writes and FindMissing calls with reads), T-2,
  T-3 and C-11 are in `crates/kbf-transfer/tests/engine.rs`, against a
  counting fake server behind a proxy that counts connections and can cut them. The
  windows are checked in the SETTINGS and WINDOW_UPDATE frames the engine sends, and
  the decode limit with a 4.5 MiB message.

---

## 11. Options considered and rejected

| Option | Why not (now) |
|---|---|
| **FUSE/NFSv4 lazy input root** (Buildbarn's virtual mode, its recommended one) | Rootless Podman would need `/dev/fuse` passed through and a FUSE server per lease. macOS needs macFUSE (no longer open source) or an NFSv4 server written in Rust. Buildbarn's own notes say lazy reads need prefetching (ADR 0010) and timeout compensation to perform. It is several times the size of C1. Revisit only if actions read a small fraction of huge input roots. |
| **Buildbarn's two layers** (a persistent block store under a volatile hard-link cache) | Two caches to size and keep consistent. kbf materializes files anyway, so the file directory *is* the store. |
| **Copy every input from the cache** (no links) in containers | Correct, but writes the zig tree's full size to the NVMe per lease. Kept as the fallback mode. |
| **Bind-mount the whole cache read-only into each container** | Exposes every cached blob of every build to every action, and the paths do not match the input tree. |
| **A cache for Linux native with clone or copy** | The cache directory is writable by a same-uid action, so the stat check and lazy verify can both be beaten (5.2b). |
| **Hard links on the native driver** | Actions run as the daemon's user, and the macOS sandbox allows writes in the lease directory. |
| **Two daemons sharing one cache directory** | Each would rebuild its own byte total and LRU, neither would count the other's inserts, and pins would not cross. One owner, by lock. |
| **fsync on every insert** | About 20,000 fsyncs on a cold zig fetch; lazy verify catches a torn file after a restart. |
| **Hash every cached file on every use** | Costs the tree's size per lease. Replaced by safety by construction, a stat check per hit, and a lazy hash once per start. |
| **Local cache answers FindMissing** | The server is the truth. A blob collected server-side but held locally would never be re-uploaded. |
| **Hot set in the Hello** | Churns `report_hash`. A bounded Heartbeat field instead (C5a). |
| **Owner = subordinate id 1 for cache files** | `fs.protected_hardlinks` stops the daemon linking files it does not own. |
| **A pool of connections to kbf-server** | One HTTP/2 connection carries many streams (4.2); a pool can be added later if a measurement ever shows the one connection is the limit. |
| **Wipe the cache at start** (Buildbarn's hard-link cache) | Daemons restart on every deploy, so every node would go cold each time. `tmp/` plus a rename, the index and lazy verify make keeping it safe (6.8). |
| **A startup scan of `blobs/`** (rebuild the LRU and total from the files, with the cache off until it ends) | Every restart would run uncached, or cold, for as long as listing 400 GB takes. CEO decision (2026-10-13): no blocking startup scan; a background adopt pass is allowed; an index loaded at start (6.9). The adopt pass never gates a lease. |
| **An append log over the snapshot** (revision 4) | A fresh or discarded index stayed cold until the first compaction; a short write mid-log lost every later record, and a dropped batch could double-count; the unindexed-file bound was false, and a deleting sweep made a lost index 400 GB to refetch (6.9). Adopting files makes one whole-file snapshot enough. |
| **Deleting files the index does not list** | A lost snapshot would turn the whole cache into deletes and refetches, and a deleting pass races fills. The adopt pass adds them instead (6.9). |
| **An embedded store for the index** (redb, SQLite, sled) | 6.9: a second recovery procedure to trust, repair on open after a crash; SQLite is not Rust. |
| **Engine and batching in one PR** | Puts an unmeasured gain and a server dependency on the path to C1 (section 9). |
| **Per-call in-flight limits** (stage A as it stands) | They multiply by concurrent leases; replaced by one process-wide limit in T1. |

---

## 12. Open questions (each with a recommendation)

- **Q1. An action that edits one of its own input files in place gets EACCES.** With the
  cache, a container's input file is a hard link to the shared cache file, which the
  action cannot write; today the same open quietly gets a private copy. Compilers and
  linkers read inputs and write new outputs, so they never hit this. A tool that patches
  an input where it lies would. An output that is also an input still works (5.1).
  Decided (CEO, 2026-10-13): accept it, as Buildbarn does. Buildbarn's virtual mode
  refuses an open for write or truncate with EACCES
  (`pkg/filesystem/virtual/blob_access_cas_file_factory.go`, `VirtualOpenSelf`), a size
  change with EACCES and a chown with EPERM (`VirtualSetAttributes`; chmod is still
  allowed there, by a TODO, but it does not make the file writable). Its native mode
  writes inputs 0444 or 0555 with a fixed mtime (`pkg/cas/blob_access_file_fetcher.go`).
  Add an opt-out (a per-action platform property `kbf-writable-inputs=1` that gives that
  action private copies) only if a real build breaks.
- **Q2. Locality placement (C5b).** *Recommendation:* ship C5a's report only, and decide
  with a week of `InputStats` from the farm.
- **Q3. Default budget.** Decided (CEO, 2026-10-13): no default in kbf (no flag, no
  cache). The farm's deploy config sets an LRU of 400 GB max per node and keeps the
  free-space floor (100 GB to start, tuned from `cache_evictions`).
