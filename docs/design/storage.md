# Storage

Storage holds the build cache: blobs in the content-addressable store (CAS) and
results in the action cache (AC). Its job is to keep two promises a build cache must
never break:

1. **Never serve a hit whose files are missing.** A cached result is served only if
   every blob it names is present and readable. Otherwise the lookup is a miss and the
   action runs again: slow is acceptable, broken is not.
2. **Never serve wrong bytes.** Every blob is checked against its digest when it is
   written and when it is read. An action-cache entry is written only once all its
   outputs are stored, and only by the farm, never by a client.

This document covers what the code does (`kbf-meta`, `kbf-segments`, `kbf-objstore`,
and the `Cache` in `kbf-front`) and what is **planned**.

## Three layers

```
metadata   what exists, where, last touched     kbf-meta (MetaState), behind MetaLog
   |
bytes      verify, pack into segments           kbf-front (Cache), kbf-segments
   |
objects    segments in a bucket                 kbf-objstore (ObjectStore)
```

- The **metadata** layer answers "is this blob present?" and "is this action cached?"
  from memory. It never asks the object store whether something exists.
- The **byte** layer verifies uploads, packs them into segments and writes them.
- The **object** layer is any store that implements the `ObjectStore` trait.

Bytes never enter the metadata layer; the object store never decides anything.

## Metadata

`kbf_meta::MetaState` is a pure state machine. It holds:

- the **CAS index**: each digest maps to a `Location` (a store id, an object id, and
  the offset of the blob's record in that object) and the farm time of its last touch;
- the **action cache**: each action digest maps to an `ActionRecord` (the digest of the
  stored `ActionResult` and its *closure*) and the time of its last hit;
- the objects found unreachable, each with the reason: `Missing` (the store did not
  produce the object or the range) or `Corrupt` (the bytes failed their digest). A
  `Corrupt` mark is never downgraded to `Missing`;
- the committed farm time;
- the next writer epoch.

It changes only through `Command`s applied in order: `Tick`, `AllocEpoch`, `PutBlob`,
`PutBlobs`, `PutAction`, `Touch`, `ObjectUnreachable`, `ObjectReachable`, `Collect`.
Reads are queries against the committed state.

An object id is a writer epoch and a sequence number. `AllocEpoch` returns an epoch
greater than every one before it; a cache takes one when it opens (`Cache::open`) and
numbers its objects from 1 within it. A `PutBlob`, `PutBlobs` or mark that names an
object of an epoch never allocated is refused and changes nothing. `PutBlobs` records a
list of blobs as one entry: it applies exactly as one `PutBlob` per blob in order, and
all or nothing. A `Location`'s store id is 0, the configured store, today; a read of a
location in any other store fails `INTERNAL`.

The front reaches it only through the `MetaLog` trait: `commit` a command and get back
what applying it did, or run a read-only `query`. `MemoryMetaLog` is one state behind a
lock in the server process. `RaftMetaLog` (`--meta-log=raft`, in
`kbf-server/src/raft_log`) exists today as a **single-voter** Raft log on the local
disk in `--raft-dir`, and needs `--store=s3`: `commit` encodes the command, proposes
it, syncs it, applies it and only then answers. Commits are grouped: commands that
arrive while a sync is in progress are written together and made durable by the next
single sync, and each is answered after it; a lone command is committed at once. The
commit and its sync run on tokio's blocking pool, not on the caller's worker;
`query` reads the server's applied state, which for a single voter, always its own
leader, is the committed state. It has no snapshots yet (the log is never
compacted) and no second voter. **Planned**: more voters, on which `commit` on a
follower sends the command to the leader and `query` runs behind a read index where
a stronger guarantee is needed.

### Three answers, not two

A blob is one of:

| Answer | Meaning | Becomes |
|---|---|---|
| `Present(location)` | held, at a readable object | served |
| `Unavailable` | held, but its object could not be read | `UNAVAILABLE` on a read; "missing" in FindMissingBlobs |
| `Absent` | not held | `NOT_FOUND` |

Only `Absent` ever becomes `NOT_FOUND`: the farm never turns "I could not read it" or
"I could not ask" into "it does not exist". `FindMissingBlobs` reports an unavailable
blob as missing so the client uploads it again; the new upload *heals* the index entry
to point at the new copy. The empty blob is always reported present and reads as no
bytes, whether or not anyone uploaded it.

### The closure check

An `ActionRecord`'s closure is every blob the result needs besides itself: each output
file, stdout and stderr, each output directory's `Tree` blob and root `Directory`, and
every file named in those trees. A lookup is a hit only if the entry has not expired
and the result blob and every closure blob are `Present`. Any other state is a miss
(`NoEntry`, `Expired`, `Absent(digest)`, `Unreachable(digest)`).

The same check guards writes: `PutAction` is refused unless its role is `Daemon` and
every blob in the record is present and reachable. Clients cannot reach this path at
all; `UpdateActionResult` answers `PERMISSION_DENIED` before reading the request.

### Retention and touches

`Retention` holds three durations, in farm time:

| Field | Default | Meaning |
|---|---|---|
| `min_ttl` | 7 days | a blob reported present or served stays held at least this long after the report |
| `touch_quantum` | 1 day | a read commits a touch only if the entry's last touch is at least this old |
| `action_ttl` | 30 days | an action entry without a hit for this long is a miss and is collected |

A read that reports or serves something returns a `Touch` listing the entries whose
last touch is at least one quantum old. The read records the touch and answers; it
waits for no metadata commit (`kbf_front::TouchConfig`). A background task commits
what is recorded, each entry once, as `Touch` commits of at most 4096 entries, every
second or as soon as a batch is full; a commit that fails is retried at the next one.
An entry expires only after its TTL **plus** one quantum, so a read that skipped the
touch because the entry was touched recently is still covered for the full TTL.

A recorded touch pins its entry. `Cache::collect` commits every recorded touch before
its `Collect`, and a read queries and records under a lock the collection holds
exclusively, and the background task holds the same lock shared while it commits, so
a collection either sees a read's touch or ran before the read's query. Within one
server process, nothing a read reported is collected within `min_ttl` of the report.

The pin has limits:

- A process that stops, by a crash or a graceful shutdown, loses the touches not yet
  committed; such an entry keeps its last committed touch, and may be collected that
  much earlier than the report promised. Normally that is about a second's touches, but
  touches pile up for as long as their commits fail, so the loss is bounded only by
  how long the metadata log refused them.
- The lock is per process. A collection on one server does not see touches recorded
  and not yet committed on another (a follower that forwards its touches to the leader
  in batches, [deployment-topology.md](deployment-topology.md)).

No collection runs today (below); the scheduled collector and the multi-server design
must account for both.

Uploads still commit their touch before they answer: `Cache::store_blobs` skips a
blob already present only once its touch is committed, and stores it again if a
collection removed it first.

`Collect` removes every expired blob and action entry and returns what it removed,
with the location of each blob's bytes, so their space can be accounted as dead.

Today, under `--meta-log=raft`, the server commits a `Tick` every second while the log
is open (`raft_log::TICK`), so the metadata's farm time advances and continues across
restarts. Under `--meta-log=memory` nothing advances it, and the index is lost at
restart. In neither mode does the server run `Collect`, so nothing expires while it
runs. **Planned**: collection runs on a schedule.

## Writes

`Cache::store_each` (and `store_blobs`, over it) is the only way bytes enter the
store:

1. Every blob is a `VerifiedBlob`: its SHA-256 and length were checked against the
   digest the client sent. A mismatch is `INVALID_ARGUMENT`.
2. Blobs already present are touched, not written again.
3. The rest are pushed into the one **open segment** shared by every concurrent upload
   (write grouping, [write-path.md](write-path.md) section 3), of at most 128 MiB
   (`MAX_SEGMENT_BYTES`). It is sealed when the next blob does not fit, or 2 ms
   (`--write-linger`) after its first blob. A blob of 16 MiB or more
   (`--write-own-segment-bytes`), or too large for an empty segment, is written as a
   segment of its own, with one record at offset 0 and a footer like any other, so
   every object in the store carries the digest and CRC of what it holds.
4. Each sealed segment is written with `put_new` (tried up to 3 times on a transport
   failure or a 5xx answer, each time under a new key; `--store-put-attempts`), then
   its footer is **read back from the store**, and the location of every blob is taken
   from the stored footer.
5. Only then are the blobs' locations committed: one `PutBlobs` per segment. Every
   upload with a blob in it is acknowledged after that; a segment that fails answers
   its own blobs with its error and no others.

Uploads are admitted under `--write-bytes-in-flight` (2 GiB) before they buffer, and
at most `--write-puts-in-flight` (4) segment PUTs run at once.

**Planned** ([write-path.md](write-path.md)): daemon uploads of an action's outputs as
whole segments with one commit, and a local write tier that syncs segments to a
server's NVMe or SSD, acknowledges, and flushes them to the object store in the
background.

So a blob is never reported present before its bytes are durable in the store.

**ByteStream uploads** follow one rule per stream: bytes arrive in order from offset 0,
are verified, become durable, and only then is the write acknowledged. There is no
partial resume; a broken stream starts again from 0. `QueryWriteStatus` reports a
blob's full size once it is durable and 0 before, never bytes a live stream has merely
buffered. An upload of a blob already durable is answered at once. Until chunked
uploads exist, a ByteStream write is held in memory until verified, so blobs over
1 GiB (`MAX_BLOB_BYTES`) are refused before any byte is buffered. Batch calls carry at
most 4 MiB of blob data.

Object keys are `<prefix>cas/<epoch>/<seq>`, each as 16 lowercase hex digits. Two
caches over one metadata log hold different epochs, so they never name the same key.
Under `--meta-log=memory` the metadata is in memory and starts again from epoch 1 at
every start, so with `--store=s3` each server start writes under its own prefix (the
configured prefix plus the start time), and keys never collide across restarts. Under
`--meta-log=raft` the epochs are committed in the Raft log, so every start writes
under the one prefix the log committed (the configured prefix plus the log id) in an
epoch no earlier start used.

## Reads

`Cache::read_blob` answers from the index, then reads exactly the blob's byte range
from its object (`get_range` at the record's offset, for the digest's size) and checks
the SHA-256 of what came back. If the store says the object does not exist, or the
range is not there, the object is marked unreachable as `Missing`; if the bytes fail
their digest, as `Corrupt` (`ObjectUnreachable`). Either way the read fails
`UNAVAILABLE`. The bad bytes are never served,
and the blob is never reported absent.

`Cache::read_blobs` serves `BatchReadBlobs`: one index query and one touch for the
whole batch, then the blobs whose records lie back to back in one object are read with
one `get_range` covering them all, and up to 32 such reads run at once
(`BATCH_READS_IN_FLIGHT`). Each blob in a joined read is checked against its own
digest. A blob whose joined read fails, or whose bytes in it fail their digest, is read
again alone as `read_blob` reads it, so it is marked and fails exactly as above, while
its neighbours, whose bytes checked, are served.

`GetActionResult` runs the closure check, touches what it serves, then reads and decodes
the stored `ActionResult`. A result blob that cannot be read or decoded is a miss.
Outputs are never inlined into the response.

## Segments

A segment (`kbf-segments`) packs many blobs into one object, so the store holds a few
large objects instead of many small ones. The layout, all integers little-endian:

```
records   blob bytes, back to back, from offset 0, uncompressed
fan-out   256 x u32: fanout[b] = number of index entries whose hash[0] <= b
index     N x 52 bytes, ascending by (hash, size):
            hash [32] | size u64 | offset u64 | crc32c u32
trailer   32 bytes:
            index_offset u64 | entry_count u32 | digest_function u8 | reserved [3]
            | footer_crc32c u32 | version u32 | magic "KBFSEG\r\n"
```

- A record's length is its digest's size; its CRC-32C is in the index.
- `footer_crc32c` covers the fan-out, the index and the first 16 bytes of the trailer.
- A reader that knows the object's length fetches the 32-byte trailer, then the
  footer, then exactly the records it needs. The fan-out narrows a lookup to the
  entries sharing the hash's first byte.
- Pushing a blob a segment already holds is a no-op; every pushed blob is hashed, so a
  record never sits under the wrong digest.
- The segment reader checks a record's length, then its CRC-32C (cheap, catches
  storage damage), then its SHA-256. The cache's ranged read checks the SHA-256 of the
  bytes it got, which covers the length too.

**Chunking.** `kbf-segments` also implements FastCDC content-defined chunking for
blobs of 8 MiB or more: chunks of 128 KiB minimum, 512 KiB average, 2 MiB maximum,
described by a `Manifest` (the blob's digest and its chunks' digests, in order).
Because a cut depends only on nearby bytes, two versions of a large file share every
chunk away from an edit. The gear table, masks and sizes are part of the stored
format, pinned by a golden test. The cache does not use chunking yet (**planned**: large
uploads stored as chunks plus a manifest, and streamed instead of held in memory).

## The object store interface

`kbf_objstore::ObjectStore` is the whole contract kbf asks of a store. One value
addresses one bucket.

| Call | Use |
|---|---|
| `capabilities()` | what the store claims beyond the base contract |
| `put_new(key, body, retain_until)` | write an immutable object, optionally under a retention date |
| `get_range(key, range)` | read the bytes of a range that exist; a range starting past the end is an error |
| `delete(key)` | delete; deleting an absent key succeeds, so a retry is safe |
| `list(prefix, after, max_keys)` | one page of keys, ascending; for rebuilds and sweeps only |

Because kbf names every key and records every location in its own metadata, it needs
no conditional writes, no consistent listing and no read-after-write from the store.
Two capabilities are optional and, when claimed, checked:

- `conditional_put`: `put_new` refuses an existing key.
- `object_lock`: an object written with a retention date refuses `delete` until then.
  A store without it must refuse a retention date rather than store the object
  unlocked.

Keys use only `A-Z a-z 0-9 - _ . ~ /`, with no empty, `.` or `..` segment, so no URL
normalisation between kbf and a store can turn one key into another.

**Implementations:**

- `MemoryStore`: in memory; the executable form of the contract and the store tests
  use.
- `S3Store`: the S3 REST API with path-style addressing and SigV4 signing of every
  payload. A ranged read answered with anything but the exact range is refused. On a
  store with Object Lock, delete names the exact object version, so a retained object
  is refused rather than hidden behind a delete marker. Not built yet: TLS endpoints and
  multipart upload (one PUT carries up to 5 GiB; segments are at most 128 MiB).

**Conformance suite.** `kbf_objstore::conformance::run` checks a store's base contract
(round trip, ranged reads at the boundaries, a missing key, idempotent delete, list
paging) and every capability it claims. A backend is trusted only once it passes. CI
runs the suite against the in-memory store and against MinIO and RustFS containers,
each with a plain bucket and an Object Lock bucket.

## Capabilities

`GetCapabilities` answers with `kbf_front::server_capabilities`, which reads no farm
state: the answer depends only on whether the front also serves `Execution`, which is
fixed when the service is made. The call is authorized against the request's instance
name like any other, and a refusal fails it rather than answering with empty
capabilities.

| Field | Value |
|---|---|
| digest functions | SHA-256 only |
| `action_cache_update_capabilities.update_enabled` | `false`: only the daemon path writes the action cache ([The closure check](#the-closure-check)) |
| cache priorities | one range, `[0, 0]`: retention follows the last touch alone ([Retention and touches](#retention-and-touches)), so a priority is accepted and changes nothing |
| `max_batch_total_size_bytes` | 1 GiB (`MAX_BATCH_TOTAL_BYTES`); a client sends the smaller of this and its own setting |
| symlink absolute path strategy | `ALLOWED` |
| compressors | none (zstd is **planned**, below) |
| API versions | 2.0 to 2.3 |
| execution (only with `Execution`) | enabled, SHA-256; no execution priorities and no node properties advertised. QoS is never a property ([scheduler.md](scheduler.md#qos)) |

## Planned

- **Replicated metadata.** `MetaState` applied from a Raft log kept on each voter's
  local disk, with its snapshots: one voter first (the single-voter log exists, without
  snapshots, see [Metadata](#metadata)), three on three hosts later
  ([deployment-topology.md](deployment-topology.md)). Each snapshot is also copied to
  the object store, so a host whose disk is lost can be rebuilt from it; with a single
  voter, what was committed after the newest copied snapshot is lost with the disk.
  The first deployment is one server. At three servers, any server ready to serve
  reads answers `ByteStream.Read`, `BatchReadBlobs`, `FindMissingBlobs` and
  `GetActionResult` from the metadata it has applied, and accepts upload bytes: it
  verifies them and writes the segments to the object store, then sends the leader
  the metadata commit. Only the leader commits; a read's touch is sent to it the same
  way. A follower may be behind, so it may answer a false "missing" (the client
  uploads again; harmless), but never a false "present": object collection's grace
  period far exceeds the replication lag, so the bytes of a blob a lagging follower
  still lists are still in the store, and where a stronger guarantee is needed the
  follower waits until it has applied up to the leader's commit index (a read index). If the leader cannot be reached, an upload's
  commit or a read index fails `UNAVAILABLE`, never a guess
  ([deployment-topology.md](deployment-topology.md#reads-and-uploads-on-every-server)).
  Daemons' blob reads are spread across all the servers the same way, on their worker
  listeners.
- **Garbage collection.** Collection marks space dead; a segment is deleted only when no
  entry uses it, through a condemn step with a delay during which any touch revives it.
  Sparse segments are compacted. A periodic sweep removes orphan objects by comparing
  the bucket with farm state, never by age alone.
- **Pins.** Named retention roots (for example a failure's logs) that collection never
  removes until unpinned.
- **Several stores.** Each location names a store id, bucket and key, so reads follow
  the location and a mover can copy live segments to a new store, verify them, commit
  the new location, then retire the old copy. Changing stores becomes a configuration
  change plus a background copy.
- **Compression.** zstd for ByteStream, advertised only once reads and writes decode it.
- **Locality.** Daemons keep a local cache of hot inputs and report what they hold, and
  each server caches hot blobs above the object store. The local write tier of
  [write-path.md](write-path.md) is the first part of that: blocks it wrote stay as a
  read cache after they are flushed. The daemon side, and the one
  transfer engine all bulk blob movement goes through, are designed in
  [worker-cache.md](worker-cache.md).
