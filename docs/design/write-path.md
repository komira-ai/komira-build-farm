# The write path: segment upload, grouping and a local write tier

How bytes get from an upload into the store, and what an acknowledgement promises.
Today's path is in [storage.md](storage.md#writes). Built: **write grouping**
(section 3: concurrent small REAPI uploads share one segment, PUT and metadata commit;
`kbf-front/src/cache/write_group.rs`) and the **local write tier** (section 6: segments
synced to a server's NVMe or SSD, acknowledged, and flushed to the object store in the
background; section 6.7 says how it was built and where it differs). **Planned**:
**segment upload** (the daemon sends an action's outputs as whole segments over the
worker listener, with one commit).

## 1. The problem, measured

On a pilot node, one action's output was a toolchain of 15,189 files (307 MB). Its
upload took 45 minutes and then failed with
`CAS call for blob .../156925448 failed: Unavailable: object store: request to the store failed`.

What the code does with that output today:

- The daemon uploads each file with its own ByteStream `Write`
  (`kbf-daemon/src/cas.rs`), up to 64 at once (`kbf_outputs::UPLOADS_IN_FLIGHT`).
- Each `Write` buffers the whole blob, verifies it, and calls `Cache::store_blobs`
  with one blob (`kbf-front/src/bytestream.rs`). `BatchUpdateBlobs` calls it once per
  request (`kbf-front/src/cas.rs`).
- `store_blobs` (`kbf-front/src/cache.rs`) packs only **its own call's** blobs into
  segments, so each file is a segment: one `put_new`, two `get_range` calls to read the
  trailer and footer back (`read_footer`), one `Command::PutBlobs`. Under
  `--meta-log=raft` commits are proposed, synced and applied one at a time under the
  log's lock (`kbf-server/src/raft_log/mod.rs`). Only then is the caller answered.
- `kbf-objstore` does not retry: one failed PUT fails the blob, and the action.

For the toolchain that is 15,189 PUTs, 30,378 GETs and 15,189 Raft commits. The
bucket on that node sits on erasure-coded HDDs with 0.6 to 1.3 s write latency,
shared with another workload's storage. The measured throughput, about 5.6 blobs a
second, is that latency divided by the client's concurrency, plus queueing in the store.

The store's cost is per request, not per byte: make far fewer requests, and take the
slow store off the path that acknowledges an upload.

## 2. Decided, and what stays the same

- **Architecture (decided).** Uploads are written to local NVMe/SSD on the server and
  flushed to the object store asynchronously (section 6). Segment upload (section 4) is
  the daemon's write path. REAPI batching and write grouping (section 3) stay for buck2
  and other standard clients.
- **The index model (decided).** Clients hold digests: an `ActionResult` names output
  files and `Tree`s by digest, and buck2 asks for each by digest. The server's Raft
  index maps each digest to `Location { store, object, offset }`; the length is the
  digest's size. A segment ([storage.md](storage.md#segments)) is the farm's "one
  tarball": many records back to back, with an index in its footer. A flush from the
  tier to the store changes only the **object's** state (staged, then stored). No
  blob's location changes.
- **Index scale.** The index is the Raft `MetaState`'s in-memory
  `BTreeMap<Digest, BlobEntry>` (`kbf-meta/src/state.rs`): about 100 to 150 B per blob
  with tree overhead (an estimate, not measured), so 10 million blobs take about 1 to
  1.5 GB. Collection keeps it to the working set. A later option, not planned here: keep
  only digest-to-segment and read the offset from the segment's own footer index.
- **Not goals (decided):** dedup across versions, and base-plus-diff storage. Two
  toolchain versions are two sets of segments. A file whose bytes are identical in both
  has one digest and is stored once already, because a blob already present is touched,
  not written again. FastCDC chunking (`kbf-segments`) remains the planned tool for
  large single files; nothing here depends on it.
- The three answers (`Present`, `Unavailable`, `Absent`), the closure check, and
  "never report a blob present before its bytes are durable" all hold. Section 6.3
  changes what *durable* means while the tier holds a segment. That is the one real
  trade in this design, stated there plainly.

## 3. Write grouping (REAPI clients)

### 3.1 Shape

A `WriteGroup` sits between the callers (`Write`, `BatchUpdateBlobs`, anything that
calls `store_blobs`) and `put_segment`. It has one **open segment** (a
`SegmentWriter`) and the callers waiting on it.

1. A caller's blobs are verified and checked against the index as today (present ones
   are touched, not written). The rest go to the group with a one-shot reply channel.
2. The group pushes them into the open segment. A digest already in it is a no-op push
   (`SegmentWriter::push` already does this), and both callers wait on that segment.
3. The open segment is **sealed** when the next blob would not fit (`WriteError::Full`)
   or when its oldest blob has waited `--write-linger` (in milliseconds, default 2). Each
   segment's linger timer is set when it opens and seals that segment only: a segment
   sealed full before its timer fires leaves the next one to its own timer.
4. A sealed segment is written as today: one `put_new`, the footer read back, one
   `Command::PutBlobs` listing every blob in it. Then every caller with a blob in it is
   answered. A caller whose blobs span two segments is answered when both are settled.

Latency: grouping itself adds at most the linger. After the seal a segment also waits
for a PUT slot (`--write-puts-in-flight`), the commits queued ahead of it and, with the
tier on, the shared block's sync.

No caller is answered before its segment's `PutBlobs` has committed. Without the tier
the guarantee is today's: **an acknowledged blob is in the object store and indexed in
the committed metadata log**.

### 3.2 Large blobs

A blob of `--write-own-segment-bytes` (default 16 MiB) or more bypasses the group and
is written as a segment of its own (one record, settled like a grouped segment: the
PUT with its retries, the footer read-back, one `PutBlobs`), so it neither waits for a
linger nor holds up small writes. It is still held in memory until verified
(`MAX_BLOB_BYTES`, 1 GiB) until the tier lets it stream to disk (section 6.2).

### 3.3 Bounds and backpressure

- **Admission.** `--write-bytes-in-flight` (default 2 GiB) is an async semaphore
  counted in bytes. A ByteStream `Write` reads its first message (which carries the
  resource name, and so the blob's size), then takes a permit for that size **before it
  reads any more**, so the bytes it buffers in `received` are covered. `received` is
  allocated at that size up front, so what is held is what was admitted, not a grown
  buffer's capacity. `BatchUpdateBlobs` takes a permit for its total before it
  calls `store_blobs`. Own-segment blobs count too. The server refuses to start with a
  value below `MAX_BLOB_BYTES`, so a single legal blob can always be admitted. A caller
  over the bound waits; nothing is refused for load.
- **What it does not cover.** A `BatchUpdateBlobs` request is decoded by tonic before
  its handler runs: up to the 4 MiB message limit per stream, times the HTTP/2
  concurrent-stream limit. That limit is set explicitly on both listeners and named in
  the flag reference, so the total is a stated number, not "bounded": 200 streams per
  connection (`MAX_CONCURRENT_STREAMS`). A `Write`'s first message (which names the
  blob) is decoded the same way, and a `Write` waiting for admission leaves up to its
  stream's flow-control window, 1 MiB (`STREAM_WINDOW_BYTES`), received and unread.
- **Flow control.** The connection's window is set to every stream's window at once
  (200 MiB, `CONNECTION_WINDOW_BYTES`) on both listeners. With the transport's default
  of 1 MiB, the unread data of the `Write`s waiting for admission on one connection
  takes the whole window, and the admitted `Write`s on it, which must finish before
  anyone else is admitted, never receive another byte: the uploads deadlock. The
  admission test below goes red that way with the windows left at their defaults.
- `--write-puts-in-flight` (default 4) bounds concurrent segment PUTs. A segment
  sealed while all are busy waits; new blobs go to a new open segment.
- Commits stay one at a time. Each now carries up to a segment's worth of blobs.

### 3.4 Failure

- **PUT fails.** It is retried up to `--store-put-attempts` (default 3) on
  `Unavailable` and timeouts only, with backoff (100 ms, then 400 ms), each retry under
  a **new object id** (the next sequence number). A retry never reuses a key, so a
  first PUT that the store kept but reported as failed cannot make the retry fail on
  `AlreadyExists` under `conditional_put`. The object it may have left is an orphan for
  the planned sweep ([storage.md](storage.md#planned)). If every attempt fails, the
  segment's blobs are not indexed and each of them is answered `UNAVAILABLE`. A
  footer read-back that does not reach the store is `UNAVAILABLE` too, and not retried.
  "Unavailable" here is a request that did not complete (connect, reset, timeout) or a
  5xx answer; any other error (a 4xx, `AlreadyExists`) fails the segment at once.
- **Per-blob answers.** `BatchUpdateBlobs` reports a status per blob, so a request whose
  blobs landed in two segments gets a mixed answer: `OK` for blobs in the committed
  segment, `UNAVAILABLE` for the other. A ByteStream `Write` has one blob and one answer.
- **Read-back disagrees** (trailer or footer does not parse, or a blob is missing from
  it): `INTERNAL` for the segment's blobs, nothing indexed, not retried. A store that
  acknowledges and then returns something else is a fault to surface.
- **Commit fails** after a good PUT: the commit's error for the segment's blobs,
  nothing indexed, the object an orphan.
- A caller that disconnects while waiting changes nothing: its blobs are still written
  and indexed with the segment.

### 3.5 Footer read-back: kept

Two GETs per segment, not per blob: the only check that the store holds a parseable
object of the length we wrote before locations are committed into it (SigV4 refuses a
body damaged in transit, not one the store failed to keep). With the tier it runs in
the flusher, off the ack path.

## 4. Segment upload (the daemon, worker listener)

The daemon knows an action's whole output set at once. Instead of asking the server to
group thousands of single-blob calls, it builds the segments itself and uploads them as
one unit. This is kbf-specific and served only on the worker listener, under its
admission rules ([worker-protocol.md](worker-protocol.md#blobs-on-the-worker-listener)).

**Built (rollout stage 2), direct path only.** The service is
`kbf.worker.v1.Segments` (`crates/kbf-proto/proto/kbf/worker/v1/segments.proto`,
served by `crates/kbf-front/src/segments.rs`); the daemon's side is `Spool` and
`Cas::put_spool` in `kbf-transfer` (`spool.rs`, `segments.rs`), and the native driver
stores its outputs that way. Where the code differs from the text below:

- The daemon spools every output as its walk stores it, and asks FindMissing for the
  whole spool afterwards: the walk reads and hashes a file in one pass, so a blob the
  server already holds costs a local copy, not a send.
- The daemon splits uploads at 1 GiB + 1 MiB (`SEGMENT_UPLOAD_BYTES`), the smallest
  `--segment-upload-max-bytes` a server starts with, so every server admits them and
  the daemon needs no flag for the server's cap.
- `Begin` is admitted for its whole length by the cache's `--write-bytes-in-flight`
  (section 3.3), the bound ByteStream and batch uploads share, and the server refuses
  to start with `--segment-upload-max-bytes` over it, or over the 4 GiB - 1 one
  admission counts (`MAX_ADMISSION_BYTES`, a semaphore's most permits per call), so a
  `Begin` never holds more than the bound counts for it. Each segment of a commit is
  stored as a grouped segment is: under a `--write-puts-in-flight` slot, its PUT
  retried under a new key (section 3.4), its footer read back; the one `PutBlobs` is
  timed as a segment commit.
- The idle time is `--segment-upload-idle-secs`. `Commit`'s reply lists the digests
  committed; a failed commit is the call's status.
- An upload's connection is found by the peer address of the worker listener's
  tracked connections (`Connections`); a connection's close is seen when the server
  drops its stream.
- `Locate` and `ReadRange` are served, and the daemon's coalescing reader
  (`Scope::read_located`) is built and tested, but cache fills do not use it yet.
- The container driver asks `Cas::missing` once for every digest of a lease's outputs
  (the files, every Directory, the Tree), copies only the missing blobs into a spool in
  the lease's scratch directory, `--transfer-in-flight` files open at a time, and
  stores the spool with `put_spool`. A digest `missing` answered is not asked again by
  the spool's upload. Its stdout and stderr are still stored blob by blob.
- A retried upload (4.2) does not ask FindMissing again: it sends a fresh `Begin` of
  the same plan. Blobs a lost-reply commit already indexed are sent and stored again,
  and `PutBlobs` keeps the existing entry, so a retry costs bytes, not correctness.
- There is no `Abort`. An attempt that failed before its `Commit` (a `PutRange` call
  failed) keeps its admission until its connection closes or it goes idle
  (`--segment-upload-idle-secs`, default 60 s). With the default bound of 2 GiB, the
  retry's `Begin` of a large upload, and other uploads of any kind, can wait that
  long. A failed `Commit` returns its admission when it ends.
- The spool copies the whole output set onto the daemon's scratch disk, with no size
  limit of its own, including blobs the server already holds: an action's outputs take
  twice their size on scratch until the upload ends.

### 4.1 Shape

1. The daemon hashes every output file (it already does, to build the `ActionResult`)
   and calls `FindMissingBlobs` once for the set, in batches under the request limit.
2. It writes the missing blobs with `kbf_segments::SegmentWriter` into one segment, or
   several when they pass `MAX_SEGMENT_BYTES` (128 MiB). A blob larger than that is a
   segment of one record, as a large blob is in section 3.2. The segment's **footer is the
   manifest**: (hash, size, offset, crc32c) per record, in the format the store already
   reads. (It is not the chunking `Manifest` of `kbf-segments`.) Records are written to
   a spool file on the worker's disk, not held in memory.
3. `kbf.worker.v1.Segments/Begin { segments: [{ length, footer }] }` opens an upload
   and returns its id. The server refuses a footer that does not parse, an entry past
   `index_offset`, overlapping records, or a total over the caps of section 4.2. It
   keeps each entry's (hash, size, offset) and ignores the daemon's crc32c.
4. `Segments/PutRange { upload, segment, offset, bytes }` calls carry the record bytes,
   up to `--segment-upload-streams` (default 16) at once as HTTP/2 streams on the
   daemon's one connection, each range up to 4 MiB. A range is marked received only
   after its bytes are written (in memory, or `pwrite` returned). A range outside the
   records, or overlapping a received range with other bounds, is refused. A resend of
   a received range (same offset and length) gets `OK` and **writes nothing**.
5. `Segments/Commit { upload }` moves the upload to **committing**: every later
   `PutRange` is refused (`FAILED_PRECONDITION`, nothing written). The server waits for
   in-flight range writes, checks every range is present, then checks **every record's
   SHA-256 against its entry**, computing its crc32c in the same pass. It stores each
   segment with a **footer it builds itself** from the checked entries and its own
   crc32c values, sorted by digest (on the tier, section 6; else one `put_new` plus
   read-back, as section 3), then commits **one** `PutBlobs` (or `PutStaged`) listing
   every record of the upload. The reply lists each digest's outcome. A second `Commit`
   (a retry after a lost reply) waits for the first and returns its result; it never
   stores or commits again. Only then does the daemon write the `ActionResult`.

A daemon cannot poison the cache this way. Nothing writes to a committing upload, so the
bytes hashed are the bytes stored; a record that does not match its entry fails the
commit (`INVALID_ARGUMENT`, nothing indexed), as a mismatched ByteStream write does
today; and a daemon's false crc32c never reaches a stored footer, where a scrub or index
rebuild would call a good record corrupt. A blob that became present between step 1 and
the commit is indexed once; its new record is dead bytes, reclaimed with the segment.

### 4.2 Bounds and failure

- **Caps.** `--segment-upload-max-bytes` is at least one `MAX_BLOB_BYTES` record and its
  footer (1 GiB + 1 MiB). Without the tier, ranges are held in memory, so the server
  refuses to start unless the cap is at most `--write-bytes-in-flight` and at most
  4 GiB - 1, the most one admission counts (default cap 1 GiB + 1 MiB). With the tier on, ranges go to disk as they arrive, so the cap may be
  high: default 16 GiB, and the server refuses to start unless it is at most
  `--write-tier-bytes`. Either way a legal `Begin` is always admitted.
  `MAX_UPLOAD_RECORDS` (16,384) bounds `Begin`'s message (16,384 entries of 52 B, about
  850 KiB, under `MAX_MESSAGE_BYTES`) and the upload's one `PutBlobs` or `PutStaged`
  (about 1 MiB in the Raft log, an estimate). The daemon splits a larger output set into
  several uploads under both caps, and writes the `ActionResult` after the last commits.
- **Memory and disk.** Without the tier, ranges are held in memory until commit, so
  `Begin` takes a `--write-bytes-in-flight` permit for its total. With the tier, `Begin`
  **reserves its total** against `--write-tier-bytes`, or uses the direct path when the
  reservation does not fit; ranges are written into the block file as they arrive
  (`pwrite`), and the permit covers only ranges in flight. A commit turns the
  reservation into staged bytes; a drop releases it.
- An upload belongs to the worker connection that called `Begin`. When it closes, or
  after `--segment-upload-idle-secs` (default 60 s) with no call, an upload that is still
  **receiving** is dropped: its permit and reservation are released and its tier file
  deleted. Nothing points at it. An upload that is **committing** is never dropped: the
  commit runs in its own task, detached from the connection and from the `Commit`
  handler's future, until it is indexed or has failed; only then is it released (a
  failed commit deletes its file, a committed one is staged). Test: close the
  connection after `Commit`, at each point (during hashing, after the sync, after
  `PutStaged` is proposed, during the direct PUT); every outcome is either nothing
  indexed and no file, or indexed with its file present.
  A daemon that reconnects calls `FindMissingBlobs` again, so blobs a commit indexed
  before its reply was lost are skipped.
- A failed `Commit` (store, commit or hash) indexes nothing. The daemon retries the
  upload once, then falls back to per-blob ByteStream, as for `UNIMPLEMENTED`.
- A server restart drops open uploads (6.4 deletes their files); the daemon starts over.

### 4.3 Downloads: segment-range fills

The same layout serves reads. For a cache fill of a `Tree` or an action's inputs, the
daemon calls `Segments/Locate { digests }` and gets `(object, offset, length)` per
blob. Blobs that share an object and sit close together are coalesced into large
ranges (gaps under 1 MiB are read through and dropped), and `Segments/ReadRange
{ object, offset, length }` calls (at most 4 MiB, as `PutRange`; longer is
`INVALID_ARGUMENT`) run in parallel on the one connection, through the transfer engine
of [worker-cache.md](worker-cache.md#4-the-shared-transfer-engine), served from the
tier when the object is staged or has a `.seg` copy, else from the store. The daemon
checks every blob's SHA-256 as today. Object ids are stable across a flush, so a
`Locate` answer stays valid; a range the server can no longer serve is `NOT_FOUND` for
that call and the daemon falls back to per-blob reads.

## 5. Capabilities (REAPI clients)

- `max_batch_total_size_bytes` is **already** advertised as `MAX_BATCH_TOTAL_BYTES`
  (1 GiB, `kbf-front/src/capabilities.rs`). buck2 sends batches of the smaller of this
  and its own `max_total_batch_size`, so a client's setting is the batch size and this is
  the ceiling; blobs at or over that size go through ByteStream in messages of that size
  (buck2 allocates a buffer of that size per upload in flight). A setting of 64 MiB
  (`[buck2_re_client] max_total_batch_size = 67108864`) keeps a batch to a few streams'
  worth of work: one batch is one call on one stream, so a very large one serialises
  what several calls would carry in parallel. buck2 sizes its decode limit as twice the
  batch size, so the server's message limit must stay at least that.
- **zstd.** Advertise `supported_compressors` and `supported_batch_update_compressors`
  = `[ZSTD]`, and serve `compressed-blobs/zstd/...` on ByteStream (refused today in
  `kbf-front/src/wire.rs`), in the same change that decodes it, since a client that
  sees it will send it. Stored bytes stay uncompressed; compression is wire-only.

## 6. The local write tier

### 6.1 Shape

With `--write-tier-dir=<path>` (on NVMe or SSD) and `--write-tier-bytes=<budget>`, a
sealed group or a committed segment upload is not PUT. It is written to a **block
file** and synced, then committed. A block holds a segment's records, so a blob's
offset in the block is its offset in the object the block becomes (Buildbarn's
`LocalBlobAccess` does the same: append to large local blocks, ack after the append).

1. A block is named by its `ObjectId` (writer epoch, sequence number) as a flat file,
   `<dir>/<epoch>-<seq>.blk`. Creating it, and writing `<dir>/TIER`, is followed by
   `sync_dir` before any record in it is acknowledged.
2. **Grouped writes** append to the one open block. Each group reserves its byte range
   in order, writes, `fdatasync`s, then commits `Command::PutStaged { tier, objects:
   [{ object, end, blobs }] }`. It records each blob at `Location { store: CONFIGURED,
   object, offset }` and marks the object **staged on `tier`**. The state machine sets
   the object's committed length to `max(len, end)`, so commits that finish out of
   order never move it backwards, and it refuses `PutStaged` for an object that is
   sealed or flushed. A group whose commit fails leaves a gap of dead bytes below a
   later group's `end`; `Footer::parse` already tolerates gaps (it checks only that
   each entry ends at or before `index_offset`, `kbf-segments/src/layout.rs`).
3. **Segment uploads** are each their own block, written whole (section 4.2), synced,
   and committed with one `PutStaged` covering every object of the upload, each marked
   sealed in that same commit. Its file holds **records only**, at `Begin`'s offsets,
   and `end` is the declared `index_offset`; `Begin`'s footer is never written. So both
   kinds of block are records up to the committed length, with no footer. Callers are
   answered after the commit.
4. **Seal.** An open block is sealed at the segment limit or after
   `--write-tier-seal-after` (default 30 s). Sealing first stops new reservations, then
   waits until every reserved range has been answered (committed or failed), then
   commits `Command::SealStaged { object }`. After that, `PutStaged` for the object is
   refused. The flusher re-reads every record, checks its SHA-256, computes its crc32c,
   builds the footer from the committed blob list (sorted by digest, `index_offset` =
   committed length; section 4.1's builder), appends it, PUTs the object at its usual
   key, reads the footer back, and commits `Command::ObjectFlushed { object }`. Then it
   renames the file to `<epoch>-<seq>.seg` and syncs the directory: a flushed copy.
5. **Reads.** The location answer carries the staged state from the same metadata
   query: `BlobAnswer::Present { location, staged_on }`. A staged blob is read from its
   block. `Cache::fetch` (`kbf-front/src/cache.rs`) and the batch read fallback must
   never commit `ObjectUnreachable { Missing }` for an object that was staged when the
   query ran; a local read that fails with ENOENT **after** `ObjectFlushed` falls back
   to the store. A flushed object may still be read locally from its `.seg` file.
6. **Eviction** deletes only `.seg` files (renamed once `ObjectFlushed` applied in
   committed state, never on a flusher-local flag), least recently read first; a staged
   block is never evicted. The budget counts unflushed staged bytes plus open uploads'
   reservations (section 4.2). At the budget, new writes fall back to the direct path
   (section 3), and a metric says so. **If the store is also down**, that fallback fails
   too: `UNAVAILABLE` until the store returns or the flusher frees space.

The index gains one small table (built: `kbf-meta`, `state/staged.rs`): staged objects,
each with its tier, committed length, state (open, sealed, conflict) and blob list. It
holds only what has not been flushed, so it is bounded by the budget. An object leaves
it on `ObjectFlushed`, or on `ObjectUnreachable` (its block is not to be trusted, and
the restart sweep deletes it). Each object that left is remembered, with the farm time
it left, for the blob retention (`min_ttl` plus the touch quantum), so a late or retried
`PutStaged` cannot stage it again; `Collect` forgets older ones. That is a stated
limit, not an accident: a commit that arrives more than the retention after its object
left would stage it again (a test pins it), and it is accepted because no forward or
retry lives that long. `SealStaged` for an object that was never staged answers
`NotStaged`, so a block whose every group failed to commit has nothing to seal; PR 4 must
treat that block as an orphan for the restart sweep. `tier` is the server's
tier id, committed once by `Command::AddTier { server, tier }` the first time a
directory is opened, and written to `<dir>/TIER`. A server has one tier at a time:
`AddTier` with another id is refused while that server's old tier holds a staged
object (objects on other servers' tiers do not count; the
abandon flag marks them unreachable first), retires the old id, and a retired id is
never registered again.

A `PutStaged` is all or nothing. Besides the refusals above, it is refused for an
object marked unreachable or staged on another tier, an object named twice, or a record
that ends past `end`. A `PutStaged` on a sealed object that adds nothing (every blob
already listed, `end` within the committed length) is a retried commit and is accepted,
so a forward retried after a sealing commit does not fail an indexed upload. A digest
an object already lists is indexed at the listed offset, so the index never names a
location the footer lacks.

**`Fs` needs work first.** `kbf-store`'s `Fs` is one flat directory with whole-file
`read` only (`kbf-store/src/fs.rs`). Flat block names fit it; a 128 MiB whole-file read
per blob read does not. PR 3 adds a ranged `read_at` and a positioned write to `Fs` and
`FaultFs`, so the crash tests run through the same trait.

### 6.2 Large blobs on the tier

A ByteStream `Write` over `--write-own-segment-bytes` streams into its own block as it
arrives (bytes come in order from offset 0), hashed as written, synced, and committed
as a staged object of one record. Memory per stream is then one message. The 1 GiB
`MAX_BLOB_BYTES` refusal can be relaxed later for the tier path; the direct path keeps it.

### 6.3 What an acknowledgement means now

**With the tier on, an acknowledged blob is on one server's local disk (synced, its
name synced) and in the committed metadata log. It is not yet in the object store.** It
survives a process crash, a restart and a power loss of that host. It does **not**
survive losing that disk before the flush: about 30 s (the seal delay) plus one PUT,
longer while the store is down. Metrics report unflushed bytes and the oldest unflushed
block's age; an alert fires on the age.

Today's deployment is one server whose Raft log is on its local disk (`--raft-dir`).
Losing that host already loses metadata committed since the last copied snapshot
([storage.md](storage.md#planned)). The tier adds a second thing on that host that a
disk loss takes, and no new host whose failure loses data.

**Later, with three servers** ([deployment-topology.md](deployment-topology.md)): the
receiving server sends each block's bytes to the tiers of two others before it
commits; `PutStaged` names every tier that synced them, and the ack waits for two, so
one lost disk loses nothing. Until then the guarantee is the one-disk one above.

### 6.4 Crash and restart

Every start takes a new writer epoch (`Cache::open`), so a block from an earlier start
never gets another append. At open, the server checks `<dir>/TIER` first:

- **`TIER` missing or naming another id while objects are staged on this server's
  tier:** the server **refuses to start**. A mount mistake or a wrong flag must not
  destroy the index. Only `--write-tier-abandon=<id>`, naming the old tier, makes it
  commit `ObjectUnreachable { Missing }` for every object staged there and register a
  new tier. The blobs become `Unavailable`: `FindMissingBlobs` reports them missing and
  a re-upload heals the index ([storage.md](storage.md#three-answers-not-two)); an action
  entry naming one is a miss. Nothing is `NOT_FOUND` and no wrong bytes are served.
- **`TIER` matches:** for each staged object, a file of at least its committed length
  is truncated to that length (bytes past it were never answered) and, if sealed,
  queued for the flusher; an unsealed one is sealed. A **single** file missing or short
  marks only that object `ObjectUnreachable { Missing }`, with an alert.
- **Orphans.** Then every `<epoch>-<seq>.blk` from an older epoch whose object is not
  staged is deleted and the directory synced: an upload open at the restart, a block
  synced before its `PutStaged` committed (neither was acknowledged), and a flushed
  object's `.blk` from a crash before the rename (the store holds it). `.seg` files stay.

Crash and conflict points in the flusher:

- After the PUT and before `ObjectFlushed`: the block is still staged and is flushed
  again under **the same key**. The footer is deterministic (committed list sorted by
  digest, crc32c from the bytes, for both kinds of block), so the key holds the same
  bytes. On `AlreadyExists` (`conditional_put`) the flusher reads the footer back; if it
  matches, it commits `ObjectFlushed`.
- If it does not match, the flusher commits `Command::FlushConflict { object }`: the
  object **stays staged**, its blobs stay `Present` and are read from the local block,
  the flusher stops retrying it, and an alert fires. It is not marked `Corrupt`, because
  `ObjectUnreachable { Corrupt }` makes `s.blob()` answer `Unavailable` and would stop
  reads of good local bytes. An operator resolves it; the block is never evicted.
- A record whose SHA-256 fails in the flusher's re-read means the local disk damaged
  it. The block is not uploaded; it is marked `ObjectUnreachable { Corrupt }` (reads
  stop, as they should), and its blobs heal by re-upload.

### 6.5 Log format

`PutStaged`, `SealStaged`, `ObjectFlushed`, `FlushConflict` and `AddTier` are new
`kbf-log` command variants (built), `MetaCommand` fields 10 to 14, and they make **log
format 2** (`kbf_log::Format::V2`), as `log.proto` requires of any added command. An
older binary already refuses them: an unset command decodes as
`DecodeError::UnknownCommand` (`kbf-log/src/meta.rs`), the Raft machine stops on it
(`kbf-server/src/raft_log/machine.rs`), and opening the log returns
`RaftLogError::Decode` (`decoded`, `raft_log/mod.rs`). This binary refuses them under
format 1 the same way, on encode and decode. A Raft log enters format 2 by one
committed `MachineVersion` entry naming machine version 2 (`kbf_log::MachineVersion::V2`,
whose format is `Format::V2`), which `RaftMetaLog::raise_machine_version` commits
(rollout stage 5, section 6.8); every later entry is encoded and decoded as format 2,
and a binary that runs only version 1 refuses the log at that entry. Tests in
`kbf-log/tests/tier_codec.rs` and `kbf-server/tests/raft_log.rs` pin both. The snapshot
encoding ([metadata-snapshots.md](metadata-snapshots.md)), not built yet, gains the
staged table, the tier registry and the record of objects that left.

### 6.6 A server and a daemon on one host

The daemon's input cache (one read-only file per blob, so leases can hard-link it,
evicted by use) and the server's write tier (segments, evicted only after their flush)
stay two stores: a hard link cannot point into a segment, and one eviction policy would
get one side wrong; a store-down backlog in the tier must never evict a running build's
inputs. They share the disk with budgets that add up and leave wide headroom, because
the kernel behaves badly under disk pressure (writeback stalls, allocation failures):
on a pilot node's 923 GB disk, input cache 200 GB, write tier 200 GB, a 100 GB free-space
floor, and the rest for lease scratch and the OS. A daemon miss for a blob staged on its own host's server is served from
that server's local tier over loopback, no object store and no LAN. On a filesystem
with reflink (XFS, btrfs) a later step may clone the byte range into the cache file
(`FICLONERANGE`) instead of copying it.

### 6.7 As built (rollout stage 4b)

The disk layer is `kbf-tier`; the rest is `kbf-front` (`cache/tier.rs`, `cache/flush.rs`)
and `kbf-server` (`write_tier.rs`). `Cache::attach_tier` is section 6.4's start: the
abandon flag, the `TIER` check and orphan sweep (`Tier::open` against a copy of the
committed staged table), `AddTier` after `TIER` is synced, and each lost block marked
`Missing`. The write group's settle offers each sealed segment to `Writer::stage`
(reserve, write, sync, `PutStaged`) and PUTs it when the tier does not take it. `Cache::flush_tier` is one round of 6.1.4: `SealStaged` for
each block `poll_seals` names, then up to four sealed objects of this tier from the
committed state, oldest first, each re-read and checked, given its footer
(`kbf_segments::layout::encode_footer` over the committed list), PUT at its key, its
footer read back, `ObjectFlushed` committed, and only then renamed to `.seg`. The
server runs a round every second. Tests: `kbf-front/tests/write_tier.rs` (the tier rows
of section 9 except the log-format row, which stage 3 pins), `kbf-tier/tests/flushed.rs`.

Where it differs from the sections above, and why:

- **The memory log clears the directory.** Under `--meta-log=memory` a restart starts
  with an empty index whose writer epochs start from 1 again, so the server deletes
  every file an earlier start left in the directory (`kbf_tier::clear`) before the
  open: none of it is indexed, and an old block's name could be one of the new start's
  objects. Under `--meta-log=raft` (allowed since section 6.9) nothing is cleared: the
  recovery above runs against the committed staged table.
- **Grouped writes reach the tier through the group's settle.** Each segment the
  write group seals (section 3) is staged as one range of the open block, with one
  `PutStaged`, before any PUT slot is taken; a segment the tier does not take is PUT as
  before. A blob written as a segment of its own gets a block of its own
  (`reserve_upload`, sealed by its `PutStaged`), buffered whole as on the direct path;
  streaming it into its block (section 6.2) is not built. Segment uploads (stage 2)
  are not merged; a `Begin` will reserve through the same `reserve_upload`. A segment
  with no blob (the open one a blob too large for it leaves behind) is neither staged
  nor PUT.
- **Any tier error before the commit goes direct**, not only the budget: `ENOSPC` or an
  I/O error on create, write or sync drops the reservation (deleting an upload's block)
  and PUTs the segment (the scenarios' question on `ENOSPC` below the budget).
- **A read error in the flusher's re-read is retried** with the PUT backoff (1 s,
  doubling to 60 s); only a digest mismatch marks the object `Corrupt`.
- **Flushed copies live in what the budget leaves over.** A `.seg` is kept while the
  staged and reserved bytes plus the copies fit `--write-tier-bytes`; a reservation
  evicts copies, least recently read first, before it is refused. So the directory's
  bytes stay within the budget.
- **A new tier's id is the writer epoch of the start that registers it**: epochs are
  allocated once each, so no id is drawn twice, and no random source is needed.
- **The oldest unflushed age** of a block recovered at a restart counts from that
  restart.
- **A `PutStaged` whose commit applied but whose answer was lost** (a log that
  stopped after the entry was durable) would leave the tier's committed length below
  the index's: the block's reads refused, the object marked unreachable by the first
  read, and the block removed at its seal. Section 6.9 closes it.

### 6.8 As built (rollout stage 5: the tier in the cell)

`kbf-sim-cell` runs the tier over the Raft log, with the scenarios of
[write-tier-scenarios.md](write-tier-scenarios.md) that stage 4b makes testable
(section 4 of that file lists each test and the planted defect it caught).

- **The format raise.** A log is raised by one `MachineVersion` entry: machine version
  2 implies format 2 (the entry carries the version alone; metadata-snapshots.md
  section 4 left the choice open). `RaftMetaLog::raise_machine_version(V2)` commits
  it if the log is below 2 and commits nothing otherwise; the metadata machine replays
  it on every start, so the format comes from the log, never from the process. A new
  log stays at version 1 until something raises it: a server that never attaches a
  tier writes a log an older binary still reads. `RaftMetaLog::open_running(.., runs)`
  opens as a binary that runs versions up to `runs` would; the server runs
  `raft_log::RUNS` (version 2). A version past `runs`, committed or asked for, is
  refused.
- **The cell's server attaches the tier at each start** as `Cache::attach_tier` does,
  after the raise, and stages, flushes and reads as `cache/tier.rs` and
  `cache/flush.rs` do, over `kbf-tier`'s `Tier` on a second `FaultFs` that a crash
  takes down with the raft disk. The cell drives `kbf-tier`, `kbf-meta` and
  `RaftMetaLog` themselves; the order of steps in `kbf-front`'s cache (sync before
  `PutStaged`, `ObjectFlushed` before the rename) is the cell's copy of it, because the
  cell polls each future once and `kbf-front`'s tier calls run on the blocking pool.
- **`kbf-server` refused `--meta-log=raft` with `--write-tier-dir`** until section 6.9.

### 6.9 As built (the tier over the Raft log in `kbf-server`)

- **The raise at the tier's start.** `kbf_server::write_tier::attach` asks the log to
  ready itself and the directory for the tier (`TierLog::ready_for_tier`) before
  `Cache::attach_tier`, and only when `--write-tier-dir` is given. The Raft log raises
  itself to machine version 2 (`RaftMetaLog::raise_machine_version`, which commits
  nothing on a log already there); the memory log has nothing to raise and clears the
  directory instead. A raft server without a tier never
  raises its log, so an older binary can still read it; once raised, a binary that
  runs only version 1 refuses the log at the raise (section 6.5).
- **No clearing under the Raft log.** The tier's directory is recovered against the
  committed staged table at each start (section 6.4); only the memory log, whose index
  starts empty, clears it (section 6.7).
- **A lost `PutStaged` answer.** When the commit answers an error, the cache reads the
  committed state: if it holds the object through the range's end, the range is
  answered committed, so the tier's committed length follows the index (its blobs are
  read from the block, and the block is sealed and flushed). The caller still gets the
  error. If the state cannot be read either, the range is answered as failed, as
  before; that cannot happen with the single-voter Raft log, whose queries read the
  state it has applied even after it stopped.
- Tests: `kbf-server/tests/write_tier_raft.rs` (the raise once, none without a tier,
  staged, sealed and flushed objects across a restart), the tier over each log in
  `kbf-server/tests/binary.rs`, and
  `kbf-front/tests/write_tier_flush.rs::a_put_staged_that_applied_but_lost_its_answer_is_read_and_flushed`.

## 7. Expected effect on the 15,189-file upload

Estimates from the measured store latency, to be measured on the pilot at each stage.

| stage | PUTs | read-back GETs | Raft commits | on the ack path | expected time |
|---|---|---|---|---|---|
| today | 15,189 | 30,378 | 15,189 | every PUT | 45 min, then failed |
| 1. grouping | about 240 | about 480 | about 240 | about 240 PUT rounds | about 5 to 10 min |
| 2. + segment upload | 3 | 6 | 1 | 3 PUTs in parallel | under a minute |
| 3. + tier | 3 (flusher) | 6 (flusher) | 1 `PutStaged`, then 1 `ObjectFlushed` per segment | local sync, one commit | network and hashing bound, seconds |

- **Grouping alone** groups only callers waiting at once; the daemon sends 64 `Write`s
  at a time, so segments hold about 64 blobs (15,189 / 64 is about 240), in sequential
  rounds. The PUT retry removes the single-PUT failure of section 1.
- **Segment upload** replaces 15,189 calls with one `FindMissingBlobs` pass, one
  `Begin`, about 74 ranges of 4 MiB 16 at a time, and one `Commit`. The store sees 3
  objects of up to 128 MiB, written in parallel; each pays the HDD latency once.
- **The tier** takes the store off the ack path: a store outage delays the flush, not
  the action. Grouped blocks fill to 128 MiB whatever the REAPI client's concurrency.

## 8. Rollout

1. **PR 1, grouping and admission** (`kbf-front`, new module `write_group.rs`, since
   `cache.rs` is near its size limit): the group, the PUT retry, the admission permit after
   a `Write`'s first message, the explicit concurrent-stream limit. Flags: `--write-linger`,
   `--write-own-segment-bytes`, `--write-bytes-in-flight`, `--write-puts-in-flight`,
   `--store-put-attempts`. No format change. Ships alone; measure the toolchain upload.
2. **PR 2, segment upload**: the `Segments` service on the worker listener (`Begin`,
   `PutRange`, `Commit`, `Locate`, `ReadRange`) in `kbf-proto` and `kbf-front`, and the
   daemon's spool, split, upload and fallback in `kbf-daemon`/`kbf-outputs`. Flags:
   `--segment-upload-max-bytes`, `--segment-upload-streams`, `--segment-upload-idle-secs`.
   Direct path only. Downloads through the transfer engine follow it.
3. **PR 3, tier metadata**: the five commands in `kbf-meta` and `kbf-log`, the staged
   table, `BlobAnswer::Present { staged_on }`, the refusal test, snapshot encoding if
   snapshots have landed (they have not). Nothing writes them yet.
4. **PR 4, the tier**: the `Fs` extension, block files, append and sync, seal, the
   flusher, local reads, eviction on committed state, upload reservations, restart
   recovery and the orphan sweep, the abandon flag, metrics. Flags: `--write-tier-dir`,
   `--write-tier-bytes`, `--write-tier-seal-after`, `--write-tier-abandon`. Off unless
   `--write-tier-dir` is given; the OSS default is no tier, today's guarantee.
5. **PR 5**: the tier in `kbf-sim-cell`, then on the pilot with the unflushed-age alert
   in place first. The scenarios it must run, their invariants and expected outcomes,
   and the store faults and crash points already in the cell, are in
   [write-tier-scenarios.md](write-tier-scenarios.md).
6. zstd on ByteStream and the capability, independent of the rest.

## 9. Tests, and the defect each catches

Grouping (PR 1), over `MemoryStore` wrapped to count and fault requests:

| Test | Catches |
|---|---|
| 64 concurrent single-blob `store_blobs` make at most a few PUTs and `PutBlobs` | grouping not engaged; mutant: seal on every push |
| a store whose `put_new` blocks: no caller returns, and `is_durable` is false, until the commit | acknowledging before the commit |
| a failing store: the segment's blobs `UNAVAILABLE` and absent; a caller in another segment succeeds | indexing a failed segment; failure spreading |
| a `BatchUpdateBlobs` spanning a failed and a good segment gets per-blob `OK` and `UNAVAILABLE` | one status smeared over the batch |
| commit refused after a good PUT: all fail, nothing present | acknowledging on PUT alone |
| PUT fails twice, then succeeds, on a `conditional_put` store: one `PutBlobs`, keys differ | retry under the same key |
| a lone write under a paused clock is sealed at the linger | waiting forever for a full segment |
| a segment sealed full before its linger: its timer, when it fires, leaves the next open segment open until that segment's own linger | a linger timer sealing whichever segment is open; mutant: the timer takes the open segment without checking its id |
| a blob at `--write-own-segment-bytes` gets its own segment without the linger | large blobs held up by small ones |
| the same digest from two callers: one record, both answered | duplicate records; a caller never answered |
| 48 concurrent `Write`s on one connection, three times the bound in all: peak buffered bytes stay under the bound, every `Write` finishes (built as 4 MiB `Write`s and a 12 MiB bound, not 64 MiB and 1 GiB, so the suite does not hash 3 GiB) | permit taken after buffering; mutant: permit in `store_blobs`; flow-control windows left at the transport's defaults (deadlock) |
| a start with `--write-bytes-in-flight` below `MAX_BLOB_BYTES` is refused | a legal blob waiting forever |
| a start with `--segment-upload-max-bytes` over `--write-bytes-in-flight`, over the 4 GiB - 1 one admission counts (`MAX_ADMISSION_BYTES`), or below one 1 GiB record plus footer, is refused; one admission of `MAX_ADMISSION_BYTES` under a 16 GiB bound is counted in full; a `Begin` over the cap or `MAX_UPLOAD_RECORDS` is refused; a `Begin` of the whole 1 GiB + 1 MiB cap is admitted under a bound of the same size, and a single-record upload that fills its bound commits (built as a 64 KiB bound and record, so the suite does not hash 1 GiB) | an upload holding more than the bound, or more than it is counted for; mutants: the start check removed, the flag's upper limit removed, `MAX_ADMISSION_BYTES` raised to 8 GiB; a commit that asks for admission again (it waits forever) |
| a `Begin` waits while other uploads hold the bound, and is admitted when they release it | segment uploads bounded apart from ByteStream and batch uploads |
| an upload's first segment PUT fails: the commit succeeds on a retry under a new key; with 2 PUT slots, a three-segment commit has 2 PUTs at the store | an upload's PUT tried once; an upload's PUTs past `--write-puts-in-flight` |

Segment upload (PR 2):

| Test | Catches |
|---|---|
| 15,000 small outputs: one `Begin`, one `Commit`, one `PutBlobs`, every blob present after | per-blob commits; mutant: commit per segment |
| one record's bytes altered after `Begin`: `INVALID_ARGUMENT`, nothing indexed | trusting the daemon's footer |
| a missing range at `Commit` is refused; an overlapping range is refused | indexing bytes never received |
| a `PutRange` with different bytes for a received range, and one sent during `Commit`, are ignored or refused; the indexed records hash correctly | verified bytes rewritten after the check; mutant: a resend overwrites |
| two concurrent `Commit`s on one upload: one store, one `PutBlobs`, the same reply | a lost-reply retry storing and committing twice |
| a record with a false crc32c in `Begin`'s footer: the stored footer has the true one and `Footer::parse` plus a per-record check pass | a daemon's crc reaching the store |
| a worker connection closed after `Begin`: the permit is free at once, not after the idle timeout | a dead daemon stalling every write |
| a `ReadRange` over 4 MiB is refused | unbounded server memory per read |
| ranges arriving in reverse order commit correctly | assuming in-order ranges |
| a server answering `UNIMPLEMENTED`: the daemon falls back to ByteStream and the action succeeds | an old server failing new daemons |
| an idle upload is dropped and its file deleted; nothing indexed | leaked tier files |
| `Locate` + coalesced `ReadRange` returns every blob of a 1,000-file tree with at most ceil(bytes / range cap) reads | fills still one call per blob |

Tier (PRs 3 to 5), over `Fs` with `FaultFs` dropping unsynced writes **and unsynced
directory entries** at a simulated crash:

| Test | Catches |
|---|---|
| acknowledged blobs read back after a crash | ack before `fdatasync`; mutant: skip the sync |
| a new block acknowledged, crash: the file still exists | ack before `sync_dir`; mutant: skip it |
| crash between append and `PutStaged`: tail truncated, nothing indexed | uncommitted bytes treated as stored |
| two groups on one block whose commits finish out of order, crash: both readable | committed length moving backwards |
| a group appended and synced but not committed when the seal fires: the seal waits; a late `PutStaged` after `SealStaged` is refused | a location indexed into an object whose footer lacks it |
| every staged blob read through `fetch` and the batch path against a store without the object: nothing marked, bytes served | a staged blob turned `Unavailable` by a reader |
| crash after the flush PUT, before `ObjectFlushed`: one object, staged mark cleared | a double flush failing, or orphaning the block |
| a segment-upload block flushed, crash before `ObjectFlushed`, reflushed: footers equal, no `FlushConflict` | a false conflict from a footer built two ways |
| a restart with an open upload, and a crash between sync and `PutStaged` on an upload block: no `.blk` left, no budget consumed | orphan files filling the disk |
| `Begin`s whose totals pass `--write-tier-bytes`: the excess goes direct; tier disk use stays under the budget | open uploads uncounted |
| a same-key object whose footer differs: `FlushConflict`, blobs still `Present` and read locally | a conflict stopping reads of good bytes |
| `ObjectFlushed` commit refused: the block is not evicted | evicting the only copy on a local flag |
| a tiny budget never evicts a staged block; at the budget writes fall back to direct PUTs | eviction of the only copy; writes refused at the budget |
| a wrong or missing `TIER` refuses start; with `--write-tier-abandon` objects become `Missing`, re-upload heals, no `NOT_FOUND` | a mount mistake destroying the index |
| one block file deleted: only that object `Missing` | one lost file failing the whole tier |
| a corrupted local record is not uploaded and is marked `Corrupt` | flushing damaged bytes |
| a log holding each new variant, opened by a reader without it, is refused | silently skipping a staged write |
| `kbf-sim-cell`: no acknowledged blob is ever absent or unreadable unless a disk-loss fault was injected, and then only as `Unavailable` | interleavings the tables above miss |

## 10. Open questions

- Linger default (2 ms), measured in PR 1.
- Packing small sealed blocks into one object: changes locations; not planned.
- With three servers, whether the ack waits for two tier syncs or one sync plus the PUT.
- Whether buck2 reads `max_batch_total_size_bytes` and would send zstd if advertised
  (section 5), to be checked on the pilot before PR 6 is prioritized.
