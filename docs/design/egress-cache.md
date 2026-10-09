# Egress cache: pinned downloads served by the farm

A build fetches files from outside: compilers from conda channels, crates, wheels,
GitHub release assets and source archives, container image layers. Every such file is
*pinned*: the build names its URL together with its SHA-256 and size. This document
designs a farm-side cache for those files, so that a build whose pins the farm already
holds never contacts an upstream host, and a new pin is fetched once, by the farm,
before the first build needs it. It covers the three cases that still reach upstream
today: a CAS miss, a pin bump and the first fetch of a new pin.

Section 2 describes the code on `main`. Everything from section 3 on is **planned**:
none of it exists yet. Open decisions are in [section 13](#13-open-decisions), each
with options and a lean. The storage it builds on is in [storage.md](storage.md); the
action sandbox is in [daemon.md](daemon.md).

## 1. Summary

| Topic | Design |
|---|---|
| What is cached | Only *declared downloads*: entries of a fetch list, each a (SHA-256, size) with candidate URLs. The (SHA-256, size) pair is the REAPI `Digest`, so a held entry is an ordinary CAS blob. |
| Who reaches upstream | One fetcher module in `kbf-server`, off unless flags name the hosts it may reach. Build actions gain no network: container actions keep `--network=none`; native actions keep what they have today, which on Linux is the node's network ([section 2.1](#21-which-farm-actions-have-the-network)). |
| How entries are kept | Each held entry is one object under a stable key prefix, read and verified at every start before it is reported present, kept while a live list names it plus a grace period. |
| How bytes get in | Fetched and verified by the fetcher, or *adopted*: a client upload of a listed digest is copied into the cache. No byte is accepted that does not hash to its key. |
| What buck2 needs | Nothing new. buck2 asks the CAS first; a held entry answers "present". |
| How a pin bump is covered | The bumping change's CI submits its list and waits until every new entry is held, failing with the entry, URL and error otherwise. |
| What a failure looks like | A state per entry in `GET /v1/mirror`, an alert naming the fix, and automatic recovery when the cause clears. |

## 2. Where things stand

### 2.1 Which farm actions have the network

- The container driver runs every action with `--network=none` and `--pull=never`
  (`crates/kbf-driver-container/src/podman.rs`, `create_args`); the test
  `the_network_is_off` catches a missing flag. [daemon.md](daemon.md) lists networked
  actions as planned only.
- The native driver reads a `network` platform property (`network_of` in
  `crates/kbf-driver-native/src/network.rs`), but enforces it only on a Mac with
  `sandbox-exec`. On Linux, and wherever `sandbox-exec` is missing, nothing is enforced:
  *every* native action has the node's network, including one that asks for none
  (`Isolation::None`: "an action that asks for no network still has it"). The node
  reports this as its `network_isolation` capability (`sandbox-exec` or `none`).
- Every server host is also a worker ([api.md](../api.md#who-may-write)), so native
  actions on a Linux server host share the host and its network with `kbf-server`, and
  with the fetcher this design adds to it. Section 5.6 says what that means for the
  fetcher.
- `kbf-server` has no HTTP client for upstream hosts. The binary's only outbound
  connection today is to the object store: it depends on `kbf-objstore`, whose
  `S3Store` uses `reqwest` (`kbf-mdm` uses `reqwest` too). The crate also contains
  `GateClient`, a mutual-TLS gRPC client to `kbf-mdm-gate`
  (`crates/kbf-server/src/mdm/client.rs`), which nothing in the binary configures yet;
  its only callers are tests. Neither reaches an upstream host, which is the claim this
  design rests on. kbf has no Remote Asset API: the vendored protos
  under `crates/kbf-proto/proto/third_party` are REAPI v2, ByteStream and their
  dependencies.
- `crates/kbf-server/src/farm.rs` caches a result when the exit code is 0 and the
  action is not `do_not_cache`, whatever its `network` property. This matters to the
  later networked actions of [section 10](#10-out-of-scope), not to this design: the
  egress cache never writes an action result.

### 2.2 The build client does the fetching

komira, the first client, fetches every external file through one rule,
`pinned_file`, which calls buck2's `ctx.actions.download_file(url, sha256, size_bytes)`.
The download runs on the buck2 client (a developer machine or a CI runner), which
uploads the bytes to the farm's CAS like any other input. Since komira-ai/komira#563
every pin states its size, and a CAS hit contacts no upstream host. That change names
three cases that still do: the CAS no longer holds the blob, a pin bump (the bumping
change fetches once), and a file written to the client's local disk. A wrong size is a
CAS miss: the CAS keys blobs by (SHA-256, size).

### 2.3 What buck2 asks the CAS

Read from facebook/buck2 `main`, not from the release komira pins:

- buck2 has no Remote Asset client. Nothing in it calls `FetchBlob`.
- `download_file` probes the CAS before downloading
  (`app/buck2_action_impl/src/actions/impls/download_file.rs`, landed 2026-09-29). It
  asks for the digest's remaining lifetime and declares the file as a CAS artifact only
  if at least 2 hours remain (`PROBE_MIN_REMAINING_TTL`); otherwise it downloads.
- The open-source REAPI client answers that question with `FindMissingBlobs`
  (`remote_execution/oss/re_grpc/src/client.rs`, `get_digests_ttl`). A present blob is
  given the client-configured `cas_ttl_secs`, 3 hours by default; a missing one 0.
- The same client caches every `FindMissingBlobs` answer, "missing" included, in an
  LRU that is cleared every 12 hours. A long-lived buck2 daemon that was told "missing"
  does not ask again for up to 12 hours. A CI job with a fresh daemon asks again.

komira pins the buck2 release of 2026-09-15, which predates the probe. komira#563
measured its behaviour: with the blob in the CAS, a build with a broken upstream URL
succeeds without contacting the host. With either release the farm's lever is the
same: answer `FindMissingBlobs` "present" for the pin's digest.

### 2.4 Today a restart empties the CAS

The index is in memory (`MemoryMetaLog`). With `--store=s3`, each start writes under
the configured prefix plus the start time (`s3_store` in
`crates/kbf-server/src/config.rs`), and object keys are `<prefix>cas/<object id>`
(`Cache::object_key`). After a restart every pin misses and every client goes back
upstream. Nothing expires while the server runs: the server binary never ticks farm
time or runs `Collect` ([storage.md](storage.md#retention-and-touches)). Named retention
roots ("Pins" in [storage.md](storage.md#planned)) are planned, not built.

### 2.5 Scale

A grep of `size` literals over komira's build rules and third-party directories finds
about 170 entries totalling about 1 GiB, the largest about 110 MiB. It counts every
platform row and is not a count of distinct pins; the export of
[section 9](#9-the-clients-side-komira) gives the exact figure. The cache is small next
to the CAS.

## 3. Terms

- **Declared download**: a file a build names by URL, SHA-256 and size. komira calls
  these pins; this document does not, because [storage.md](storage.md#planned) uses
  "Pins" for retention roots.
- **Fetch list**: a named, versioned set of declared downloads, submitted by a client
  repository.
- **Entry**: one digest named by at least one live list.
- **Held**: the entry's bytes are in the mirror store, verified, and indexed.
- **Mirror store**: the stable key prefix that holds held entries.
- **Adopt**: copy a client's upload of a listed digest into the mirror store.

## 4. Failure modes and what answers each

| Failure | Without the cache | Mechanism | Report, alert, recover |
|---|---|---|---|
| Upstream down or slow | every cold client stalls through its HTTP retries, then fails | held entries are served from the mirror store; nothing on the read path contacts upstream | a fetch that stalls is cut off by the connect, idle and minimum-throughput limits of [section 5.6](#56-the-fetcher) and counts as failed; report `fetch_failed{host, error, since}` on entries not yet held; alert after N failed attempts, naming the host, the entries at risk and the fix; retry with backoff, clearing on success |
| Upstream serves different bytes at the same URL (a regenerated archive, a moved tag, a compromised host) | builds fail everywhere until someone re-pins | bytes are accepted only if both SHA-256 and size match; a held copy is never replaced; new bytes are never accepted automatically | report `upstream_drift{url, observed_sha256, observed_size}` only for a complete 2xx body with other bytes (a cut or failed download is `fetch_failed`), or `drift_seen` on a held entry; alert "the farm still serves the held copy; re-pin to a stable asset or vendor it"; an entry not held is retried with backoff and becomes `held` when a candidate serves the right bytes or a client's upload is adopted; the alert clears when no live list names the digest ([section 5.6](#56-the-fetcher)) |
| A wrong pin in a change | that change's build fails | the same check: nothing is stored; the CI wait step fails the change naming the entry | reported on the entry; no alert, because the change's own check is the signal |
| A pin bump hits many runners at once | N clients fetch upstream together | the list is submitted before the build; one copy writer per digest across fetch, adoption and healing ([section 5.2](#52-the-mirror-store-and-its-location)); a per-host concurrency cap; the first verified copy, fetched or adopted, serves everyone and later writers skip | in-flight count and queue depth per host; no alert unless a fetch fails |
| Cache poisoning | not applicable | the digest is the only key; a URL is a hint; every byte is verified before commit and on every read; nothing is keyed by URL; the action cache is never written | counter `mirror_rejected_total{reason}` |
| The fetcher used to reach internal addresses | not applicable | HTTPS only; a host allow-list by flag; no proxy; DNS resolved once per hop and the connection made to that address; every address that is not globally routable refused at every hop ([section 5.6](#56-the-fetcher)); the fetcher fetches only listed entries, never a URL given at miss time | refusal recorded on the entry; alert naming the refused URL and address and the fix, which is another URL: the filter has no override flag ([section 8](#8-report-alert-recover)) |
| A source's licence forbids a private copy | not applicable | a per-host policy: `fetch`, `adopt` or `deny` ([section 5.5](#55-per-host-policy)) | the report shows each entry's policy; a `deny` entry is reported, not alerted |
| Store pressure | not applicable | per-list caps, a total cap, and a separate cap for lists from unmerged changes inside the total, so those lists cannot crowd out the default branch ([section 7](#7-retention-and-caps)); over a cap, new entries are refused and held entries are never evicted | alert at 80% of a cap and on any refusal, naming the cap flag and the lists that hold the most; recover by raising it or retiring lists |
| The store damages a copy as it is written, or a write or its read-back fails | not applicable | every copy is read back and re-hashed before it is committed, for fetch and adoption alike; nothing points at an unconfirmed copy, so a digest in a segment stays served from there ([section 5.2](#52-the-mirror-store-and-its-location)) | report `store_write_failed{copy, error}`, the copy recorded as damaged and prunable; alert as a mirror-store write failure with the store error and the steps it calls for; retry with backoff as the next copy, clearing on commit |
| Server restart | cold CAS | held entries live under a stable prefix and are re-indexed at start, each reported present once its bytes are verified | re-index count, verified bytes and duration in the report; alert if the listing or a read fails, naming the store error; the re-index retries by itself |
| A damaged object in the store | not applicable | every mirror object is read and hashed at start before it is reported present ([section 5.3](#53-re-index-at-start)), so a damaged object found then is never served; damage after that is caught by the re-hash on read, which fails the actions that already passed Execute's input check ([section 5.3](#53-re-index-at-start) says exactly which); a copy damaged as it is written is caught by the read-back before it is committed ([section 5.2](#52-the-mirror-store-and-its-location)) | report `corrupt`; alert with the fix; heal by writing a new copy under a new key, fetched under `fetch` or copied from a client's upload under `adopt` ([section 5.2](#52-the-mirror-store-and-its-location)) |

## 5. The mechanism

### 5.1 Fetch lists

- A list is JSON lines, one entry per line: `{sha256, size, urls[], label}`. `label` is
  free text for reports and alerts; komira's export sets it to the target that declares
  the download ([section 9](#9-the-clients-side-komira)). A list has a name and a
  generation number that only increases.
- `PUT /v1/mirror/lists/{name}` replaces a list with a newer generation; an older
  generation is refused. An equal generation is accepted, and changes nothing, only if
  its entries are the same as the held list's; with other entries it is refused. So a
  re-run CI job resubmits safely, and two different submissions cannot share a number.
  The client picks the number. komira's CI step uses the CI run number, which a re-run
  keeps and every new push to the change increases; a run that finishes after a newer
  one is refused, which is the right answer. `DELETE` retires a list. `GET /v1/mirror`
  is the report of [section 8](#8-report-alert-recover).
- Each entry must state a 64-hex-digit SHA-256 and a positive size. An entry without
  either, a size over the per-entry cap, or a URL that is not `https` is refused at
  submission with a reason, and the whole submission is refused, so a list is never
  half applied.
- Caps by flag: entries per list, bytes per list, total bytes across lists, and the
  bytes that lists with an expiry may add to the total ([section 7](#7-retention-and-caps)).
  A list over its caps is refused with the totals in the error.
- A list carries an optional expiry. CI sets one on lists from unmerged changes (the
  lean is 14 days); lists from the default branch and release branches have none and
  live until replaced or retired. A list with an expiry counts against the
  unmerged-change cap; one without counts only against the total.
- The writes go through the operator API and its gates ([api.md](../api.md#who-may-write)).
  Who may submit is an open decision ([D2](#d2-how-lists-arrive-and-who-may-submit)).
- kbf knows nothing about any client repository. A list is data.

### 5.2 The mirror store and its location

- A held entry is one whole object, a *copy*, at
  `<mirror-prefix>sha256/<hex>/<size>/<copy>`, where `<copy>` is a decimal number
  starting at 0. `--mirror-prefix` is a flag and does not change between starts. A copy
  is written with `put_new` to the number after the highest one kbf knows of, so kbf
  never knowingly writes a key twice: a new copy of the same digest gets the next
  number. (A listing that missed a key can make kbf write it again; the next bullet but
  one says what happens then.) So a damaged copy is never repaired in place; it is
  *superseded* by a verified copy with a higher number.
- **One copy writer per digest.** Every path that writes a copy (a fetch's commit, an
  adoption queued by `store_blobs`, an adoption queued by the sweep, and healing after
  `corrupt` or `store_write_failed`) first takes the digest's copy lock, one per digest
  for the whole server, so two writers never run for the same digest at once. Under the
  lock the writer looks again: if a copy of the digest is now committed, it writes
  nothing (a fetch removes its spool file, an adoption is dropped and counted as
  `mirror_adopt_skipped_total`). Otherwise it takes the next copy number under the lock;
  a number taken in this start is never taken again, even if its write failed. A writer
  that waits for the lock does not hold a fetcher or adopter slot while it waits.
- **A committed copy is never touched by another writer.** A writer records "damaged" or
  "unconfirmed, prunable" only for the number it took itself, and only while no
  `PutBlob` names that number. A committed copy becomes damaged in the report only when
  a verification of that copy fails (a read, the re-index or the scrub), which makes the
  entry `corrupt{copy}` ([section 8](#8-report-alert-recover)), never by a later write.
  On a store without `conditional_put` (`--s3-conditional-put` is off unless set), where
  `put_new` replaces an existing key, the writer first reads the key's first chunk with
  `get_range`: `NotFound` lets the write go ahead, bytes are handled as `AlreadyExists`
  below, and a store error is `store_write_failed`; so kbf does not overwrite a key a
  stale listing missed either.
- **Writing a copy, for fetch, adoption and healing alike.** The bytes going in are
  already verified (the spool's hash, or a `VerifiedBlob`). kbf writes copy N with
  `put_new`, then reads copy N back from the store by range and re-hashes it, size
  included. Only when that matches is `PutBlob { Location::Mirror { copy: N } }`
  committed; nothing points at copy N before then, so a digest held in a segment keeps
  its segment location until a verified mirror copy replaces it. What can go wrong
  after the write, and what happens:
  - The read-back does not match. Copy N is damaged by the store, not by upstream (the
    bytes were verified before the write). It is recorded as a damaged copy, prunable
    ([section 7](#7-retention-and-caps)); the entry is `store_write_failed{copy, error}`
    and alerted as a mirror-store write failure; the write is retried with capped
    exponential backoff as copy N+1.
  - The read-back or the write itself fails with a store error. The same: nothing is
    committed, the entry is `store_write_failed{copy, error}`, and the retry writes the
    next number. Copy N, if it was written at all, is recorded as unconfirmed and
    prunable; a re-index that meets it verifies it like any copy before using it.
  - The retry needs the bytes again. A fetch keeps its spool file until the copy is
    committed and re-hashes it before each retry (a spool file that no longer hashes is
    removed and the entry refetched). An adoption does not hold its bytes across the
    backoff: the entry goes back to the adopt sweep ([section 5.4](#54-adopt-on-upload)),
    which re-reads the blob verified from the CAS.
  - It clears by itself when a retry commits. `upstream_drift` is never recorded here:
    it means only that bytes fetched into the spool did not match the entry
    ([section 5.6](#56-the-fetcher)).
- Why numbered copies: `put_new` on a store that claims `conditional_put` refuses an
  existing key (`AlreadyExists`), so a fixed key per digest could never be rewritten
  after damage; on a store without it, `put_new` replaces the key
  (`crates/kbf-objstore/src/types.rs`, `Capabilities`), which is safe only because kbf
  reads the copy back and verifies it before committing it. Numbered copies behave the
  same on both kinds of store.
- A `put_new` that returns `AlreadyExists` means a copy with that number exists that
  this start did not list (the listing need not be consistent with recent writes:
  `ObjectStore::list`). kbf then reads and verifies that object: if it hashes to the
  digest, it is adopted as the entry's copy and nothing is written; if not, it is
  recorded as a damaged copy and the write moves to the next number. Either way the
  report says which.
- Mirror copies are written with no retention date (`retain_until` is `None`).
  `object_lock` is required only of the store that holds `audit`
  (`Capabilities::object_lock`), and a locked copy that turned out damaged could not be
  deleted until its date passed. A copy is protected from removal by the retention
  rules of [section 7](#7-retention-and-caps), which remove nothing silently.
- A superseded or damaged copy stays in the store and in the report until the planned
  collector removes it, after it has been reported as prunable
  ([section 7](#7-retention-and-caps)). Re-index never reads a copy that a newer
  verified copy supersedes.
- The object is not packed into a segment, so it can be listed, inspected and removed on
  its own. Entries are few (hundreds), so this costs little.
- `put_new` takes the whole body as `Bytes` (`crates/kbf-objstore/src/lib.rs`), so
  writing a copy holds the whole entry in memory, though the fetch itself is spooled to
  disk ([section 5.6](#56-the-fetcher)). Reads need not be whole: `get_range` takes a
  `ByteRange`, so every mirror read this design adds (the re-index's verification and
  the read-back after a write) reads the copy in `--mirror-read-chunk` pieces (lean
  8 MiB) and hashes as it goes. The memory bound is therefore the per-entry cap times the
  number of copies written at once (at most `--mirror-fetchers` fetches plus
  `--mirror-adopters` adoptions; an adoption holds the blob it read from the CAS), plus
  one chunk per writer for its read-back, plus `--mirror-verifiers` times the chunk. A
  retry of a store write from the spool takes a fetcher slot like a fetch, and a retried
  adoption takes an adopter slot, so retries add nothing to the bound. With the lean
  per-entry cap of 512 MiB, 4 fetchers, 4 adopters and 8 verifiers that is
  8 x 512 MiB + 8 x 8 MiB + 8 x 8 MiB, 4 GiB plus 128 MiB, in the worst case, and about
  120 MiB per write for the largest entry today. A verifier that read whole
  copies would add 8 copies of 512 MiB, 4 GiB more, which is why it reads by range. A
  streaming put (multipart upload) is later work ([section 12](#12-order-of-work));
  until then the per-entry cap is also a memory cap for writes.
- Today's `Location::Object(ObjectId)` (`crates/kbf-meta/src/model.rs`) names an object
  by a counter under the per-start prefix, so it cannot name a key that survives a
  restart. The cache adds one variant, `Location::Mirror { copy }`, whose key is
  computed from the digest and the copy number. Reachability is tracked for it the way
  `ObjectUnreachable` and `ObjectReachable` track segment objects. When the planned "several stores" location
  model lands ([storage.md](storage.md#planned)), `Location::Mirror` becomes a location
  in a named store.

### 5.3 Re-index at start

- At start `kbf-server` lists `<mirror-prefix>sha256/` (`ObjectStore::list`, paged) and
  groups the keys by digest. A key that does not parse is left alone (nothing is deleted
  at start), listed in the report under `unparsed_keys`, and alerted, naming the key and
  the fix: remove it from the store, or correct `--mirror-prefix` if the prefix is shared
  with something else. The adopt sweep ([section 5.4](#54-adopt-on-upload)) re-lists
  the mirror prefix for such keys, so the alert clears by itself within one sweep of the
  key's removal, with no restart.
- **Nothing is reported present before its bytes are verified.** For each digest the
  re-index reads its highest-numbered copy by range to the end, hashes it, and only if
  it matches
  commits a `PutBlob` with `Location::Mirror { copy }`. A copy that does not match, or
  is shorter than its key says, is recorded as damaged and the next lower copy is tried.
  A digest with no matching copy is not committed: it is `Absent`, so
  `FindMissingBlobs` answers "missing", the client uploads it, and the upload is
  adopted into a new copy ([section 5.4](#54-adopt-on-upload)), or the fetcher writes
  one under `fetch`. The entry is reported `corrupt` until that new copy is committed.
- The REAPI listener does not wait for the re-index. Until a digest is verified it is
  absent, which is today's answer after a restart, so a client that asks early goes
  upstream as it does today and its upload is adopted. Verification reads at most the
  held bytes once (about 1 GiB today, [section 2.5](#25-scale)), `--mirror-verifiers`
  objects at a time (lean 8). The report shows its progress.
- A read that fails with a store error (not a mismatch) leaves the digest absent and
  marks it `unverified{error}`; it is retried with backoff and alerted after N attempts.
- A failed listing fails the re-index: the server reports it, raises an alert naming
  the store error, starts without the mirror rather than serving a partial view as
  complete, and retries the listing with backoff. A listing that succeeds but misses
  recent keys is not detectable as such; the missed copy is not indexed, so the entry
  is fetched or adopted again; on a `conditional_put` store that write meets
  `AlreadyExists`, which [section 5.2](#52-the-mirror-store-and-its-location) turns into
  "verify and adopt the existing object", and on another store it replaces the key with
  the same verified bytes.

**Damage after verification.** Bytes can still go bad after the start-time check. The
first read after that is `Cache::fetch`, which re-hashes, marks the object unreachable
and returns `UNAVAILABLE` (`crates/kbf-front/src/cache.rs`). If that first read is a
worker fetching an action's input, the action fails: the daemon maps any CAS error but
`NOT_FOUND` to `CasError::Unavailable` (`call_error` in `crates/kbf-daemon/src/cas.rs`),
`tree_error` in `local.rs` makes that `RuntimeError::Failed`, and `lease.rs` reports it
`INTERNAL`, not a `MISSING` precondition. This is today's behaviour for any damaged
blob, segment or object, and this design does not change it: the farm never turns "I
could not read it" into "it does not exist"
([storage.md](storage.md#three-answers-not-two)). What heals afterwards:

- The entry is `Unavailable` in the index from that read on. Execute's input check
  answers a *file* input from the index (`missing_inputs` calls `Cache::find_missing`
  for files, `crates/kbf-front/src/execution.rs`), so the client's next Execute of any
  action that takes the blob as a file input is refused with a `MISSING` violation,
  which REAPI asks the client to answer by uploading. A directory input is different:
  `read_input` reads it through `Cache::read_blob`, and an unreachable directory fails
  the Execute `UNAVAILABLE` ("the input exists, so it is not MISSING"). Mirror entries
  are downloaded files, never directories, so the `MISSING` path is the one that
  applies. A buck2
  daemon that cached "present" in its `FindMissingBlobs` LRU
  ([section 2.3](#23-what-buck2-asks-the-cas)) is not consulted for that refusal; how
  buck2 handles a `MISSING` refusal is not verified here.
- The mirror heals without a client: the mark wakes the fetcher under `fetch`, which
  writes the next copy; under `adopt` the next upload is adopted into the next copy.
- The entry is reported `corrupt` and alerted, naming the digest and the fix
  ([section 8](#8-report-alert-recover)). The planned scrub
  ([section 12](#12-order-of-work)) re-reads held copies so that damage is found by the
  farm rather than by a build.

So a damaged copy found at start is never served. That it then fails no build rests on
one assumption (section 14, not verified): a buck2 daemon whose `FindMissingBlobs` LRU
still says "present" from before the restart answers Execute's `MISSING` refusal by
uploading the file. Damage that happens while the server runs fails every action that
passed Execute's input check before the index marked the blob: queued, already
dispatched, or joined as an in-flight twin of one of those. Each fails `INTERNAL` when
its lease reads the blob, because from the mark on `read_blob` answers unreachable,
which the daemon sees as `UNAVAILABLE`. Actions submitted after the mark are refused
`MISSING` instead. The damage is reported, alerted and healed.

### 5.4 Adopt on upload

- When `Cache::store_blobs` commits a digest that a live list names and the mirror
  store does not hold, the cache queues a copy of the verified bytes to the entry's next
  mirror copy. The upload is acknowledged as today, after the segment commit; the copy
  is a background write on a bounded queue, at most `--mirror-adopters` at once (lean
  4). The copy is written, read back and re-hashed before `PutBlob` moves the digest to
  `Location::Mirror`, exactly as [section 5.2](#52-the-mirror-store-and-its-location)
  says for every copy, under the digest's copy lock, so an adoption that races a fetch
  of the same digest waits and then skips if the fetch committed first; until then, and if the write fails, the digest keeps its segment
  location and is served from there.
- `store_blobs` commits only *fresh* blobs: a digest already present in the CAS is
  touched, not stored (`crates/kbf-front/src/cache.rs`). Two cases therefore get no
  upload to adopt: a list submitted after its blobs are already in the CAS (the common
  case under `adopt`, the default policy), and an adoption dropped because the queue
  was full, after which the blob is present in a segment, `FindMissingBlobs` says
  "present", and no client uploads it again. Both are closed by the *adopt sweep*: on
  every list submission, and every `--mirror-sweep-interval` (lean 10 minutes), the
  server takes each listed entry that has no committed mirror copy (including one in
  `store_write_failed` whose backoff has passed), and if the CAS holds it
  (`Present`), reads it through `Cache::read_blob` (verified) and queues the copy.
  Nothing waits for a client.
  Each sweep also re-lists `<mirror-prefix>sha256/` (keys are few) only to refresh
  the report's `unparsed_keys`; it indexes nothing from that listing.
- A full queue drops the copy, counts `mirror_adopt_dropped_total`, and marks the
  entry `adopt_pending`; the next sweep retries it. An entry that stays
  `adopt_pending` for longer than T is alerted, naming the queue flag.
- `waiting` therefore means what it says: listed, host `adopt`, and not in the CAS
  either.
- Adoption needs no egress. On its own it keeps every listed file a client has uploaded
  once across eviction and restart.
- Each adoption also counts `mirror_client_fetched_total`: a client went upstream for a
  listed file. Under `fetch` that means the bump window was lost, so the counter
  measures how well the CI wait step works.

### 5.5 Per-host policy

`--mirror-host <host>=<policy>`, repeatable:

| Policy | Fetches | Adopts | Use |
|---|---|---|---|
| `fetch` | yes | yes | sources whose terms allow a private copy |
| `adopt` | no | yes | the default for a host not named: keeps only what our own clients already downloaded; causes no new egress |
| `deny` | no | no | sources whose terms forbid keeping a copy on a server (the Xcode precedent of [fleet-updates.md](fleet-updates.md) section 7.4) |

`adopt` still keeps a copy on the farm, longer than the CAS would. That reduces the
licence question to the one the CAS already raises for any uploaded input; it does not
remove it. The default is [D3](#d3-licence-default-for-a-host-not-named).

### 5.6 The fetcher

- A module of `kbf-server`, since fewer services is the rule
  ([D1](#d1-where-the-egress-lives)). It is behind a trait so that moving it to its own
  process later does not rewrite it. It is the only part of the farm that opens a
  connection to an upstream host, and it is off unless some host has policy `fetch`.
- **The host it shares.** Every server host is also a worker, and native actions on a
  Linux host have the node's network ([section 2.1](#21-which-farm-actions-have-the-network)).
  The fetcher gives such an action nothing it does not have already: it opens no
  listener, takes no URL from a request (only listed entries, written through the gated
  operator API), and its spool and the list token are readable only by the server's
  user, which [api.md](../api.md#who-may-write) already requires to differ from the
  daemon's user and every lease's.
- **Connections.** HTTPS only, with rustls and the system roots. The client is built
  with no proxy (`reqwest`'s `no_proxy()`): by default `reqwest` honours
  `HTTP_PROXY`/`HTTPS_PROXY` from the environment, which would send the request to an
  address the filter never checked. For every hop the fetcher resolves the host once,
  refuses the answer if *any* address is not globally routable, and connects to the
  address it checked, so a second lookup cannot return something else. "Not globally
  routable" is a deny list that names every special-purpose range rather than relying
  on `Ipv4Addr::is_private`, which omits most of them. For IPv4 (prefixes written
  short, trailing zero octets left out): 0/8, 10/8, 100.64/10 (shared address space,
  used by carrier NAT and overlay VPNs), 127/8, 169.254/16 (which covers cloud metadata
  services), 172.16/12, 192.0.0/24, the documentation ranges 192.0.2.0/24,
  198.51.100.0/24 and 203.0.113.0/24, 192.88.99/24, 192.168/16, 198.18/15, 224/4
  (multicast), and 240/4, which includes the limited broadcast address. For IPv6,
  only global unicast (2000::/3) is accepted, which already excludes `::`, `::1`,
  IPv4-mapped (`::ffff:0:0/96`) and IPv4-compatible (`::/96`) addresses, NAT64
  (64:ff9b::/96 and 64:ff9b:1::/48), 100::/64, 5f00::/16 (SRv6 segment identifiers,
  RFC 9602), fc00::/7, fe80::/10 and ff00::/8; inside it 2001::/23, 2001:db8::/32,
  2002::/16 (6to4, which embeds an IPv4 address) and 3fff::/20 (documentation,
  RFC 9637) are refused too. The table follows IANA's special-purpose address registries, with a
  test per row.
  Redirects are followed up to `--mirror-max-redirects` (lean 5), each hop checked again
  against the host list and the address filter. The fetcher sends no credentials and no
  cookies.
- **Time limits.** A connect timeout (`--mirror-connect-timeout`, lean 10 s), an idle
  timeout between body reads (`--mirror-idle-timeout`, lean 30 s), and a minimum
  throughput (`--mirror-min-throughput`, lean 64 KiB/s averaged over a minute) end a
  stalled or trickling fetch as `fetch_failed{error: timed out | too slow}`, which is
  retried with backoff like any failure. No total deadline is set, because the largest
  entries are large; the throughput floor bounds the time instead.
- **Streaming.** The body is written to a spool file under `--mirror-spool-dir` on local
  disk, not held in memory, while the fetcher counts bytes and computes SHA-256. It
  aborts at `size + 1` bytes. `Content-Length` is never trusted (GitHub source archives
  send none).
- **The spool.** At start the server refuses to run if the spool directory is not owned
  by its own user with mode `0700`, naming the path and the `chown`/`chmod` that fixes
  it, as it does for the token file. Files left by a crash are removed at start; the
  count is in the report and the start line, so nothing goes silently. Before a fetch
  starts, the fetcher checks that the spool's file system has at least the entry's size
  free; if not, or if a write fails for lack of space, the fetch ends
  `fetch_failed{error: spool full}`, the spool file is removed, and an alert names the
  directory, the bytes free and the bytes needed. The retry succeeds once space is
  freed.
- **Which failure is which.** A fetch from one URL ends in one of three ways, and only
  the last is drift:
  - *Transient*, recorded as `fetch_failed{host, error, attempts, next_retry}`: a DNS,
    connect or TLS error; a connection reset or any transport error before the body is
    complete; a body that ends early by its own framing (fewer bytes than its
    `Content-Length`, or a chunked body with no final chunk); a body with no framing
    (an HTTP/1.x body delimited by closing the connection) that ends shorter than the
    entry's size, since a cut connection cannot be told from a short file; a status that
    is not 2xx (a 404 included, as a moved file and a broken mirror look the same);
    `timed out`, `too slow` and `spool full`. `error` names the class and the status or
    the transport error. Nothing is written, the spool file is removed, and the fetch is
    retried with capped exponential backoff; it clears by itself when one succeeds.
    `Content-Length` is used only to tell a complete body from a cut one, never as the
    entry's size.
  - *Unavailable for longer*: the same state after N attempts, or after T for a host
    all of whose entries fail, raises the alerts of [section 8](#8-report-alert-recover);
    the retries go on, so the host's return needs no person.
  - *Drift*, recorded as `upstream_drift`: a 2xx response whose body is complete (by its
    framing, or for an unframed body at least the entry's size, which includes the abort
    at `size + 1`) and whose length is not the size or whose SHA-256 is not the entry's.
- **Commit.** The spool check comes first: if it is drift, nothing is written to the
  store, the spool file is removed, and the observation is recorded against that URL
  with what was observed; the remaining `urls[]` are still tried, and the entry is
  `upstream_drift` only when no candidate gave the right bytes. That is the only place
  `upstream_drift` is recorded. An entry in `upstream_drift` that is not held is
  retried on the same capped backoff as `fetch_failed`, because a moved tag can move
  back and a candidate can be fixed; a retry that matches commits the copy and the
  entry is `held`. An upload of the right bytes by a client is adopted as usual
  ([section 5.4](#54-adopt-on-upload)), and the entry becomes `held` with the drift kept
  as the annotation `drift_seen{url, observed_sha256, observed_size, at}`, so the
  re-pin alert stays until no live list names the digest or a later fetch of that URL
  matches. When the spool matches, it is written as the
  entry's next copy (read into memory for `put_new`), read back and re-hashed, and only
  then committed, as [section 5.2](#52-the-mirror-store-and-its-location) says; a
  read-back that does not match, or a store error, is `store_write_failed`, retried
  from the spool, never `upstream_drift`.
- **Candidates.** `urls[]` are tried in order; each is verified the same way.
- **Concurrency.** One fetch per digest at a time (later callers wait on it), and its
  commit takes the digest's copy lock shared with adoption and healing
  ([section 5.2](#52-the-mirror-store-and-its-location)); at most
  `--mirror-host-concurrency` per host, at most `--mirror-fetchers` in all. A failure
  retries with capped exponential backoff.
- **OCI registries.** A blob URL names its digest, but a registry may want an anonymous
  bearer token even for a public image. v0 does not implement the token exchange; such
  hosts stay on `adopt` until it is built.

### 5.7 What `FindMissingBlobs` answers

- A held entry is in the index, so `Cache::find_missing` answers it from memory, as it
  answers any blob today. The new code adds nothing to that RPC: no object-store call,
  and no list lookup for digests that are absent.
- A listed entry that is not held is reported missing, as today, and the fetcher is
  woken for it. v0 never holds the RPC while a fetch runs
  ([D5](#d5-hold-findmissingblobs-while-a-fetch-runs)).
- Retention covers buck2's lifetime assumption. A present answer is taken by the
  open-source client as `cas_ttl_secs` of remaining life (3 hours by default, above the
  probe's 2-hour floor). A mirror entry is never collected while a live list names it,
  and an unlisted blob reported present stays held at least `min_ttl` (7 days) after
  the report. When collection runs, held entries must be retention roots; this design
  depends on that and the collector must honour it.

## 6. Flows

### 6.1 First fetch and pin bump

1. The change that adds or bumps a pin runs, in CI, the client's export of its fetch
   list ([section 9](#9-the-clients-side-komira)) and submits it as a list named for the
   change, with an expiry.
2. The farm diffs the new generation against what it holds and fetches the new
   digests whose host has policy `fetch`.
3. The CI step polls `GET /v1/mirror` until every entry of its list is held, for at most
   a bounded time, and fails naming each entry that is not held, with its URL, state and
   last error. A wrong pin therefore fails at submission time, not in whichever build
   meets it first. If the farm cannot be reached, or refuses the submission for a reason
   that is not the list's (a server error, a bad credential), the step fails, naming the
   farm error. It does not skip: the build after it runs on the same farm, so it could
   not pass either, and a skipped step would hide a lost bump window.
4. The build runs after that step and gets CAS hits.
5. On merge, the default branch's list is submitted with the merged pins. The change's
   list expires.

A build that starts before step 3 finishes fetches upstream itself, as today, and its
upload is adopted. A change from a fork cannot hold the submission credential, so its
new pin is fetched by the client as today and is not adopted (it is on no list) until
the merge submits it.

### 6.2 A CAS miss after eviction or a restart

Held entries are not evicted, and the re-index of [section 5.3](#53-re-index-at-start)
brings them back after a restart, each once its bytes are verified; until then it is
missing, as every blob is after a restart today. A listed digest that is still absent (its fetch
failed, or its host is `adopt` and no client has uploaded it) is reported missing as
today and the fetcher is woken.

### 6.3 Upstream outage

Held entries are served with no upstream contact. Entries not yet held fail on the
client as they do today, and the report and alert name them and their host. Recovery
needs no person: the fetcher's retry succeeds when the host returns, or any client that
can reach the file uploads it and it is adopted.

### 6.4 Upstream drift

A held entry is never refetched, so drift is seen only when a fetch runs: a new list, a
retry, or the later scrub. Drift seen on a held entry, or on an entry later held by a
fetch from another URL or by an adoption, leaves it `held` with the `drift_seen`
annotation and the re-pin alert. Drift on an entry that is not held is retried with
backoff ([section 5.6](#56-the-fetcher)); a cut or failed download is never drift. An entry that is only adopted is never compared with
upstream at all, so drift on an `adopt` host goes unseen; the report says so per entry.

### 6.5 A file written to the client's disk

komira#563's third case. With buck2 after the probe of section 2.3, a probe hit
declares the file as a CAS artifact, so writing it locally reads from the CAS and a held
entry covers this case too (read from the source, not tested). The release komira pins
downloads from the URL. Closing the case is a client decision
([D6](#d6-local-materialization-and-the-build-tools-own-download)).

## 7. Retention and caps

- An entry is kept while any live list names it, and for `--mirror-grace` after the last
  list that named it is replaced, retired or expires (lean 90 days), so older commits
  and bisects still build after upstream changes. Then it is reported as prunable and
  removed by the planned collector, never silently. Superseded and damaged copies
  ([section 5.2](#52-the-mirror-store-and-its-location)) are reported as prunable at
  once and removed the same way.
- Caps: per entry, per list and in total, all flags. Over a cap new entries are refused
  with a reason; a held entry is never evicted to make room.
- **Unmerged changes cannot crowd out the default branch.** With "never evict", many
  change lists (each living 14 days) could fill the total and leave the default
  branch's next bump refused. So the bytes of entries named *only* by lists with an
  expiry count against `--mirror-unmerged-cap` (lean a quarter of the total), and the
  rest of the total is headroom only lists without an expiry can use. An entry is
  charged to the unmerged cap only while no list without an expiry names it; when the
  merge submits it on the default branch's list, it moves to the headroom. A change
  list over the unmerged cap is refused, which fails that change's CI step naming the
  cap; the default branch is unaffected.
- The total is reported, with the share each list holds and the use of each cap.

## 8. Report, alert, recover

**Report.** `GET /v1/mirror` lists, per entry: digest, labels, the lists and
generations that name it, its host policy, its state and the last error. States:

| State | Meaning | Clears when |
|---|---|---|
| `held` | a verified copy is in the mirror store and indexed; it may carry the annotation `drift_seen{url, observed_sha256, observed_size, at}` | the annotation, with its alert, clears when no live list names the digest or a later fetch of that URL matches |
| `verifying` | at start, its copies are not yet read and hashed ([section 5.3](#53-re-index-at-start)) | the check ends |
| `unverified{error}` | a copy could not be read at start (a store error, not a mismatch) | a retried read succeeds |
| `fetching` | a fetch is running | the fetch ends |
| `waiting` | listed, host `adopt`, not in the CAS either | a client uploads it |
| `adopt_pending` | the CAS holds it but the copy into the mirror has not been written yet (queue full, or the sweep has not run) | the next sweep writes it |
| `fetch_failed{host, error, attempts, next_retry}` | the last fetch failed in transport, ended early, got a status other than 2xx, timed out or found the spool full; never a mismatch ([section 5.6](#56-the-fetcher)) | a retry succeeds |
| `upstream_drift{url, observed_sha256, observed_size, attempts, next_retry}` | not held, and every candidate that returned a complete 2xx body returned other bytes | a retry from any candidate matches, or a client's upload is adopted (both make it `held` with the `drift_seen` annotation), or no live list names the digest |
| `store_write_failed{copy, error, attempts, next_retry}` | a copy was written but its read-back did not match or could not be read, or the write failed; nothing was committed ([section 5.2](#52-the-mirror-store-and-its-location)) | a retried write of the next copy is read back verified and committed |
| `refused{host_not_allowed, address_filtered, over_cap, not_https}` | the farm will not fetch it | `host_not_allowed`: the `--mirror-host` flag changes; `over_cap`: the cap flag or the lists change; `address_filtered` and `not_https`: the entry's URLs change in a resubmitted list |
| `denied` | host policy `deny` | the policy changes |
| `corrupt{copy}` | a stored copy failed its digest, at start or on read, and no newer verified copy exists yet | a verified copy with a higher number is committed, fetched or adopted; this survives a restart, because re-index verifies before it indexes and skips the damaged copy |

Totals: held bytes, caps and their use, entries per state, re-index count, verified
bytes and duration of this start, spool files removed at start, and the superseded and
damaged copies awaiting removal.

**Metrics.** `mirror_fetch_total{host, result}`, `mirror_bytes_fetched_total`,
`mirror_bytes_held`, `mirror_adopted_total`, `mirror_adopt_dropped_total`, `mirror_adopt_skipped_total`,
`mirror_client_fetched_total`, `mirror_rejected_total{reason}`, `mirror_corrupt_total`,
`mirror_hits_total` (present answers for listed digests). `kbf-server` has no metrics
endpoint today, so v0 puts these counters in the report.

**Alerts.** Each names the entry, the host and the exact fix:

| Alert | Fix it names |
|---|---|
| an entry of a default-branch list not held after T | the URL and error; none needed if the host returns (the fetcher keeps retrying); if the URL is gone, add a working URL to the entry's `urls[]` in the client and resubmit the list, or vendor the file |
| `upstream_drift`, or `drift_seen` on a held entry | do not accept the new bytes; re-pin to a stable asset or vendor the file; the farm keeps serving a held copy and keeps retrying an entry that is not held |
| a host failing for longer than T | the host, the error and the entries at risk; the same fix as the row above: none if the host returns, otherwise another URL for those entries, or `--mirror-host <host>=adopt` and one client build that downloads them |
| `refused{host_not_allowed}` | `--mirror-host <host>=fetch` (or `adopt`) and the flag's current value |
| `refused{address_filtered}` or `refused{not_https}` | the URL and the address or scheme refused; no flag allows it (the filter has no override, by design); add an `https` URL on a public address to the entry's `urls[]` and resubmit the list, or vendor the file |
| a cap at 80%, or any refusal over a cap | the cap flag and its value, and the lists holding the most bytes, to retire with `DELETE /v1/mirror/lists/{name}` |
| re-index failed, a mirror-store read failed, or `store_write_failed` after N attempts | the store error, the key and the store flags (`--s3-endpoint`, `--s3-bucket`, `--s3-region`, `--s3-prefix`, `--s3-conditional-put`, `--mirror-prefix`, and the credential variables). The step to take follows the error: an authorization error, fix the credential the server runs with; a signature error that names the region, correct `--s3-region`; a store that refuses the conditional write, remove `--s3-conditional-put`; a missing bucket or a refused prefix, correct the flag; a store that is down or out of space, restore it. A read-back mismatch means the store returned other bytes than it was given: check the store's own health and disks, since no fix in kbf applies. Every retry is automatic and the state clears when one succeeds |
| `unverified` after N attempts | the store error and the key, with the same steps as the row above; meanwhile the digest is absent, so the fetcher (under `fetch`) or a client's upload (under `adopt`) may write a newer copy, which supersedes the unread one |
| `unparsed_keys` | the keys; remove them from the store, or set `--mirror-prefix` to a prefix used by nothing else |
| `adopt_pending` for longer than T | `--mirror-adopters` and the queue's depth |
| `corrupt` | the digest and the damaged key; under `fetch` none (the fetcher writes a new copy); under `adopt`, one client build, with a fresh buck2 daemon (`buck2 kill` first, since a running daemon may have cached "present"), of a target whose remote actions take the file as input; the alert names the declaring target from the entry's label ([section 9](#9-the-clients-side-komira)), and the build's upload is adopted. Or `--mirror-host <host>=fetch`, if the host's terms allow a copy |
| spool full | the spool directory, the bytes free and the bytes needed |

`kbf-alert` has no code yet (`crates/kbf-alert/src/lib.rs` is a module comment), and
`kbf-server` delivers no alerts. v0 exposes every alert state in the report and logs it
once when it appears and once when it clears; delivery to the operator is
komira-ai/komira-build-farm#189 (native alerting, which covers "the other planned
alerts" as well as node attention items), and these alerts go through the same path.

**Recover.** Every state in the table clears by itself once its cause is fixed; none
needs a restart or a command beyond the fix the alert names.

## 9. The client's side (komira)

- **No rule changes.** `pinned_file`, `crates_io_library` and `oci_base` already state
  (SHA-256, size), and buck2 already asks the CAS first.
- **One generic export target** writes the fetch list (SHA-256, size, URL, label) for
  every declared download in the build graph, with the label set to the target that
  declares the download, so an alert can name a target to build: the platform table, the crate rule, the
  OCI base-image rule, the Python pins and the direct `pinned_file` calls. Its test
  compares it with the `pinned_file` actions found by a graph query, so a pin added
  through a new wrapper cannot be missed. It names no farm.
- **One CI step** submits the list and waits ([section 6.1](#61-first-fetch-and-pin-bump)).
  The farm address and the credential come from CI secrets, never from source. No
  client daemon and no new CLI: the step is an HTTP request and a poll.
- **Bazel** clients could use the same cache through a Remote Asset front later
  ([D7](#d7-a-remote-asset-front)).

## 10. Out of scope

- **Unpinned installs.** A package manager resolving from a channel at run time (komira's
  release validation runs `pixi install`) is not a declared download. It needs pinning
  first, then networked actions whose only route is the cache.
- **Networked actions.** Planned in [daemon.md](daemon.md). The cache could later be
  their only egress, through content-addressed URLs; it does not provide that in v0.
- **A build tool's own download.** komira's `./buck2` bootstrap fetches buck2 outside
  the build graph, so the CAS does not help it ([D6](#d6-local-materialization-and-the-build-tools-own-download)).

## 11. Tests

Each test names the defect it catches and the mutant planted to see it red.

**Unit.**
- Fetch verifier: exact bytes; one byte short; one byte long; same size with a
  different hash; no `Content-Length`; a lying `Content-Length`. Mutants: drop the size
  cap; skip the hash compare; trust `Content-Length`.
- Failure classes: a reset after half the body, a body shorter than its
  `Content-Length`, a chunked body with no final chunk, an unframed body cut short, a
  404 and a 503 with an HTML page: each ends `fetch_failed` with its class, never
  `upstream_drift`, and the next attempt after the backoff, served the right bytes,
  ends `held`. A complete 2xx body one byte long or with another hash is
  `upstream_drift`; with a second candidate that serves the right bytes, the entry is
  `held` with `drift_seen`. An entry in `upstream_drift` then uploaded by a client is
  `held` with `drift_seen`. Mutants: record an early EOF as drift (the reset and short
  cases go red); stop retrying drift (the moved-back case never holds); leave an adopted
  drifted entry in `upstream_drift` (the adoption case goes red).
- Address filter: one case per row of the table in [section 5.6](#56-the-fetcher),
  including an address in 100.64/10, an IPv4-mapped IPv6 address of a 10/8 host
  (`::ffff:` followed by it), a NAT64 address and a 6to4 address; a DNS answer with one public
  and one private address; an `http://` URL; a redirect into a private range; a DNS
  answer that changes between two lookups; `HTTPS_PROXY` set in the environment pointing
  at a fake proxy that must see no connection. Mutants: use `is_private` alone (the
  100.64/10 and mapped cases are then accepted, so their tests go red); unwrap no mapped
  address; check only the first hop; resolve again at connect; build the client without
  `no_proxy()`.
- Time limits: a fake upstream that stalls after the headers, and one that trickles
  below the throughput floor, each ends `fetch_failed`. Mutant: drop the idle timeout.
- Spool: a spool directory with the wrong owner or mode refuses start, naming the fix;
  a full spool ends `fetch_failed{spool full}` and raises the alert; crash leftovers are
  removed and counted. Mutant: skip the free-space check.
- Lists: a resubmitted equal generation with the same entries is accepted and changes
  nothing; with other entries it is refused. Mutant: accept any equal generation.
- Caps: change lists fill the unmerged cap, and a default-branch list is still
  accepted. Mutant: charge every list to the total only.
- List validation: no SHA-256, size 0, over each cap, an older generation. Mutant: apply
  the valid half of a bad submission.
- Copies: a `put_new` meeting `AlreadyExists` on a good object adopts it and writes
  nothing; on a damaged object it writes the next copy. Mutant: treat `AlreadyExists`
  as success without reading.
- Read-back: a fake store that flips one byte of copy N as it stores it, and one whose
  `get_range` on copy N fails once. For a fetch and for an adoption, each: no `PutBlob`
  names copy N; the entry is `store_write_failed` (never `upstream_drift`) with the
  alert state set; copy N is listed as damaged or unconfirmed and prunable; after the
  backoff copy N+1 is written, read back, committed, and the state clears; an adopted
  digest is served from its segment throughout. Mutants: commit without read-back (a
  `PutBlob` names the flipped copy N, and the test's read through `Cache::read_blob`
  after it fails, so both assertions go red); record a read-back mismatch as
  `upstream_drift` (the state assertion goes red); drop the retry (the state never
  clears).
- Copy writers: a fetch and an adoption of the same digest released together on a
  `MemoryStore` without `conditional_put`, with the store set to damage the second
  write: exactly one copy is written, the other writer skips, and no committed copy is
  listed as damaged or prunable. Mutant: take the copy lock in the fetcher only (both
  write the same number, the second overwrites the committed copy, and its read-back
  marks it prunable, so both assertions go red).
- Policy: an `adopt` host is never fetched; a `deny` host is never adopted. Mutant:
  `adopt` fetches.

**Integration** (`kbf-it`, an in-process fake upstream with fault injection, on
`MemoryStore` and on the object-store conformance targets):
1. A first fetch fills the entry, and the fake upstream sees exactly one request for N
   concurrent wakes. Mutant: remove single-flight.
2. Fetch, restart (a new per-start prefix), then, once the re-index has verified it,
   `FindMissingBlobs` reports present, with zero upstream requests. Before the
   verification ends the digest is reported missing. Mutants: write under the per-start
   prefix; skip the re-index; commit the `PutBlob` before the bytes are read.
3. Adopt on upload, restart, hit, with the fetcher off. Mutant: adopt only when the
   fetcher is on.
4. A blob already in the CAS, then a list naming it: the sweep copies it into the
   mirror with no upload. A full adopt queue: `adopt_pending`, then held after the next
   sweep. Mutants: adopt only from `store_blobs`; clear the entry on a dropped copy.
5. Upstream serves other bytes: `upstream_drift`, nothing stored, the held copy
   unchanged, the action cache untouched.
6. A mirror copy damaged in the store while the server is down, then a start: the
   damaged copy is never reported present; `FindMissingBlobs` reports missing; the
   state is `corrupt`; a client upload (under `adopt`) or the fetcher (under `fetch`)
   writes copy 1, the state clears, and after a second restart copy 1 is served and copy
   0 is not read. Run on `MemoryStore` with `conditional_put` claimed and not. Mutants:
   index before verifying (a damaged copy answers present); rewrite copy 0 in place
   (refused with `AlreadyExists` on the conditional store, so the state never clears).
7. A mirror copy damaged while the server runs, then an Execute whose input it is,
   through a real daemon: the action fails `INTERNAL` (today's behaviour, asserted so a
   change to it is seen), the entry is `corrupt`, the next Execute is refused with a
   `MISSING` violation for that digest, and under `fetch` copy 1 is written with no
   client involved. Mutant: do not wake the fetcher on a mark.
8. A digest on no list: zero upstream requests and nothing adopted.
9. The cap refuses a new entry and never evicts a held one. Mutant: evict the oldest.
10. Upstream down, then up: `fetch_failed`, alert state set, then `held` with the state
   cleared. The same with an upstream that resets every response halfway: `fetch_failed`,
   never `upstream_drift`, and `held` once it serves whole bodies.

**Simulation** (a new family in `kbf-sim`, on the virtual clock, in the conventions of
[simulation.md](simulation.md)): upstream up, down and drifting at random; list
generations and client uploads interleaved; restarts; mirror copies damaged in the
store, copies damaged as they are written, listings that miss recent keys, upstream
responses cut short or answered with an error status, and fetch, adoption and healing of
one digest released at the same instant, run on a store with `conditional_put` and on
one without. After every step: a committed mirror copy
hashes to its key unless it was damaged after its last verification; no committed copy
is overwritten, or reported damaged or prunable, except after a failed verification of
that copy; after a restart no
copy is answered present before it is verified; at most one fetch per digest is in
flight, and at most one copy writer per digest; no cut or error response is recorded as
`upstream_drift`; no entry is
lost while listed or in grace; every listed entry that is not `held` carries a reason;
nothing outside the lists is fetched. At the end: every state whose cause healed has
cleared. Mutants: drop single-flight; drop the re-index; let a failed fetch clear the
entry; index a copy before verifying it; commit a copy without reading it back; take
the copy lock in the fetcher only (on the store without `conditional_put` an adoption
overwrites a committed copy); record an early EOF as drift.

**End to end**, on a deployed farm, in the shape of komira#563's proof:
1. A cold farm and a client whose upstream for one pin is unreachable: the build fails.
2. Submit the list with the true URL; the fetcher holds the entry; the same build passes
   with every action remote and no upstream contact.
3. Restart the server; the build passes again with no upstream contact.

## 12. Order of work

**v0:** the list API with validation and caps; `Location::Mirror`, the mirror store
with numbered copies and the verifying re-index; adopt on upload and the adopt sweep;
the fetcher with the host policy, address filter, time limits, spool checks,
single-flight and backoff; the report and counters; the client's export and CI step.

**Later:** alert delivery through `kbf-alert` (komira-ai/komira-build-farm#189); the
OCI token exchange; a scrub that re-reads held copies; a streaming (multipart)
`put_new`; holding `FindMissingBlobs` during a fetch; an HTTP blob endpoint by digest;
a Remote Asset front; moving the mirror index onto the replicated metadata.

## 13. Open decisions

### D1. Where the egress lives

| Option | For | Against |
|---|---|---|
| (a) A fetcher module in `kbf-server`, off by default | one service; covers first fetch on the farm | the server process gains an HTTP client and egress |
| (b) A separate fetch process | egress isolated in its own process and network policy | one more service |
| (c) No farm egress: adopt only | no egress at all | a hosted runner still reaches upstream, so an outage at bump time blocks the bump |

**Lean: (a)**, behind a trait so (b) is a later move. A deployment that sets no `fetch`
host runs as (c).

### D2. How lists arrive and who may submit

| Option | For | Against |
|---|---|---|
| (a) The operator API with its operator token | nothing new | CI then holds a token that can also cordon and drain nodes |
| (b) The operator API with a second token, `--mirror-token-file`, that may write lists only | CI holds the least it needs | one more secret to rotate |
| (c) The farm pulls lists from the repository at a commit | no credential in CI | the farm needs repository credentials and polls a source outside itself |

**Lean: (b).** Single tenant: a token holder is trusted, and the caps of section 5.1
guard against mistakes, not attackers.

### D3. Licence default for a host not named

`deny`, `adopt` or `fetch`. **Lean: `adopt`**: it keeps only bytes our own clients
already downloaded and causes no new egress, though it is still a copy on the farm.
Before a host moves to `fetch`, its terms are read; the vendor conda channel that serves
the Mojo compiler first, then the public conda channel, the crate registry, the Python
package index, GitHub release assets and the container registry.

### D4. Retention of entries no list names

| Option | For | Against |
|---|---|---|
| (a) A grace period after the last list (lean 90 days) | older commits build; bounded growth | a commit older than the grace may go upstream |
| (b) Forever, under the cap | every commit builds | growth bounded only by the cap |

**Lean: (a)**, reported as prunable before anything is removed.

### D5. Hold `FindMissingBlobs` while a fetch runs

| Option | For | Against |
|---|---|---|
| (a) Never (v0) | the RPC is never slower; simple | a build that beats the CI wait step goes upstream |
| (b) Hold up to a bound for a listed, fetching entry | the client often never fetches | client timeouts and retries for this RPC are unmeasured; a slow upstream stalls every probe; a long-lived buck2 daemon caches a "missing" for up to 12 hours anyway |

**Lean: (a)**, with the CI wait step closing the bump window. Revisit after measuring
the client's timeouts.

### D6. Local materialization and the build tool's own download

| Option | For | Against |
|---|---|---|
| (a) The client moves to a buck2 release with the CAS probe | closes komira#563's third case with no farm code | a client upgrade, tested on its own |
| (b) An HTTP endpoint serving blobs by digest, and a client URL prefix | also covers the `./buck2` bootstrap | a new endpoint and a client setting |

**Lean: (a)** for builds; (b) later, only if the bootstrap download proves a problem.

### D7. A Remote Asset front

| Option | For | Against |
|---|---|---|
| (a) Later: `FetchBlob` on the same fetcher and verifier, requiring a `checksum.sri` SHA-256 qualifier | Bazel clients use the cache with no CI step | buck2 never calls it today |
| (b) Propose upstream that buck2 call `FetchBlob` on a probe miss | no CI step for buck2 either | upstream review and a release the client pins |
| (c) Neither | nothing to build | lists stay the only way in |

**Lean: (a) later, and (b) proposed upstream.**

### D8. The first list on a running farm

Submitting the first list to a deployed farm makes it fetch and store files: a write.
**Lean:** the first submission is an operator action, approved once; after that the CI
step submits on every merge.

## 14. Verified and assumed

| Claim | Status | Source |
|---|---|---|
| Container actions run with `--network=none` | V | `crates/kbf-driver-container/src/podman.rs` |
| `kbf-server` has no HTTP client for upstream hosts (the binary's only outbound connection is `kbf-objstore`'s `S3Store`; the crate's `GateClient` to `kbf-mdm-gate` is not configured by the binary, only by tests); no Remote Asset protos are vendored | V | `crates/kbf-server/Cargo.toml`, `crates/kbf-objstore/src/s3/mod.rs`, `crates/kbf-server/src/mdm/client.rs`, `crates/kbf-server/tests/mdm_gate.rs`, `crates/kbf-proto/proto/third_party` |
| Native actions on Linux, and on a Mac without `sandbox-exec`, have the node's network whatever they ask | V | `crates/kbf-driver-native/src/network.rs` (module comment, `Isolation::None`) |
| A daemon input read that fails other than `NOT_FOUND` fails the action `INTERNAL`, not `MISSING` | V | `crates/kbf-daemon/src/cas.rs` (`call_error`), `local.rs` (`tree_error`), `lease.rs` |
| Execute's input check answers a *file* input from the index, so an unreachable file is `MISSING` there; an unreachable *directory* input fails the Execute `UNAVAILABLE` | V | `crates/kbf-front/src/execution.rs` (`missing_inputs`, `read_input`), `Cache::find_missing` |
| `get_range` reads any `ByteRange`, so a copy can be verified in chunks | V | `crates/kbf-objstore/src/lib.rs`, `types.rs` (`ByteRange`) |
| `put_new` takes the whole body as `Bytes`; without `conditional_put` it replaces an existing key | V | `crates/kbf-objstore/src/lib.rs`, `types.rs` (`Capabilities`) |
| A restart empties the CAS; nothing expires while the server runs | V | `crates/kbf-server/src/config.rs`, [storage.md](storage.md) |
| `Location::Object` names an object under the per-start prefix | V | `crates/kbf-meta/src/model.rs`, `Cache::object_key` |
| `Unavailable` is reported missing by `FindMissingBlobs`, so an upload heals the index; a worker's read of it fails the action `INTERNAL` | V | [storage.md](storage.md#three-answers-not-two), `crates/kbf-front/src/cache.rs`, `crates/kbf-daemon/src/cas.rs` |
| `store_blobs` writes only blobs not already present | V | `crates/kbf-front/src/cache.rs` |
| `reqwest` honours proxy environment variables unless built with `no_proxy()` | V, `reqwest` documentation | `reqwest::ClientBuilder` |
| How buck2 answers a `MISSING` refusal of Execute, including a daemon whose `FindMissingBlobs` LRU cached "present" before a restart; "a damaged copy found at start fails no build" ([section 5.3](#53-re-index-at-start)) rests on it | A, not read | facebook/buck2 |
| buck2 has no Remote Asset client | V on buck2 `main`, by search | facebook/buck2 |
| `download_file` probes the CAS and needs 2 hours of remaining life | V on buck2 `main` | `download_file.rs` |
| The open-source client gives a present blob `cas_ttl_secs` (3 hours default) and caches answers for up to 12 hours | V on buck2 `main` | `remote_execution/oss/re_grpc/src/client.rs` |
| The release komira pins behaves the same at the CAS boundary | A; measured for hits by komira#563, not read in its source | komira-ai/komira#563 |
| Writing a held file locally reads it from the CAS after the probe | A, read from the source, not tested | `download_file.rs` |
| A public registry blob GET may need an anonymous token | A | to be checked per registry |
| The working set is about 1 GiB | A, a rough grep | section 2.5 |
| Each source's terms allow a private copy | A, not checked | D3 |
