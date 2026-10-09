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
lock in the server process. A replicated log (**planned**) implements the same two
calls: `commit` proposes and resolves after apply, `query` runs on the leader.

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
last touch is at least one quantum old. The cache commits the touch *before* it
answers. If a collection removed an entry between the read and its touch, the touch
reports it lost and the read starts again (up to three times, then `UNAVAILABLE`). An
entry expires only after its TTL **plus** one quantum, so a read that skipped the touch
because the entry was touched recently is still covered for the full TTL.

`Collect` removes every expired blob and action entry and returns what it removed,
with the location of each blob's bytes, so their space can be accounted as dead.

Today the server binary does not advance the metadata's farm time and never runs
`Collect`, so nothing expires while it runs; the index lives in memory and is lost at
restart anyway. **Planned**: the leader commits a `Tick` every second, and collection
runs on a schedule.

## Writes

`Cache::store_blobs` is the only way bytes enter the store:

1. Every blob is a `VerifiedBlob`: its SHA-256 and length were checked against the
   digest the client sent. A mismatch is `INVALID_ARGUMENT`.
2. Blobs already present are touched, not written again.
3. The rest are packed into segments of at most 128 MiB (`MAX_SEGMENT_BYTES`). A blob
   too large for an empty segment is written as a segment of its own, with one record
   at offset 0 and a footer like any other, so every object in the store carries the
   digest and CRC of what it holds.
4. Each segment is written with `put_new`, then its footer is **read back from the
   store**, and the location of every blob is taken from the stored footer.
5. Only then are the blobs' locations committed: one `PutBlobs` per segment. The
   upload is acknowledged after that.

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
The metadata is in memory today and starts again from epoch 1 at every start, so with
`--store=s3` each server start still writes under its own prefix (the configured
prefix plus the start time), and keys never collide across restarts.

## Reads

`Cache::read_blob` answers from the index, then reads exactly the blob's byte range
from its object (`get_range` at the record's offset, for the digest's size) and checks
the SHA-256 of what came back. If the store says the object does not exist, or the
range is not there, the object is marked unreachable as `Missing`; if the bytes fail
their digest, as `Corrupt` (`ObjectUnreachable`). Either way the read fails
`UNAVAILABLE`. The bad bytes are never served,
and the blob is never reported absent.

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

## Planned

- **Replicated metadata.** `MetaState` applied from a Raft log on every server, with
  snapshots stored in the object store. Answers that need fresh state (an action-cache
  hit, "absent" on a read path) come from the leader; if it cannot be reached the
  answer is `UNAVAILABLE`, never a guess.
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
  servers cache hot blobs above the object store.
