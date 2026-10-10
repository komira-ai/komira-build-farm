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
- **Entry**: one digest named by at least one live list, or in grace.
- **Has a copy**: the mirror store holds at least one copy object of the entry's digest
  that kbf knows of, verified or not: one the re-index listed at this start, or one
  whose `put_new` succeeded in this start ([section 5.2](#52-the-mirror-store-and-its-location)).
  This depends on the store, not on the entry's state.
- **In grace**: when its last list stops naming it, an entry that has a copy stays an
  entry *in grace* for `--mirror-grace` ([section 7](#7-retention-and-caps)), whatever
  its state, except `denied`, which leaves, its copies already prunable (`waiting` never
  has a copy, [section 5.4](#54-adopt-on-upload)). It is served, healed and reported
  like a listed one, and the list records that last named it stand in for its live
  lists: it keeps their URLs, label and policy, and its drift notes stay live. At a
  start, grace is rebuilt from the store ([section 5.1](#51-fetch-lists)): an entry is
  in grace if a list record within grace (a retired one, an expired one, or a
  generation a newer one replaced) named it, no live list names it, and the store holds
  a copy object of its digest, even one a failed write left unconfirmed, which is
  verified like any copy before it is used.
- **Live entry**: an entry named by a live list, or in grace. An entry *leaves* the
  report, with its alerts, when it is neither: no live list names it and it has no copy,
  or its grace ends.
- **Held**: the entry's bytes are in the mirror store, verified, and indexed.
- **Mirror store**: the stable key prefix that holds held entries.
- **Adopt**: copy a client's upload of a listed digest into the mirror store.

## 4. Failure modes and what answers each

| Failure | Without the cache | Mechanism | Report, alert, recover |
|---|---|---|---|
| Upstream down or slow | every cold client stalls through its HTTP retries, then fails | held entries are served from the mirror store; nothing on the read path contacts upstream | a fetch that stalls is cut off by the connect, idle and minimum-throughput limits of [section 5.6](#56-the-fetcher) and counts as failed; report `fetch_failed{host, error, since}` on entries not yet held; alert after N failed attempts, naming the host, the entries at risk and the fix; retry with backoff, clearing on success |
| Upstream serves different bytes at the same URL (a regenerated archive, a moved tag, a compromised host) | builds fail everywhere until someone re-pins | bytes are accepted only if both SHA-256 and size match; a held copy is never replaced; new bytes are never accepted automatically | report `upstream_drift{url, observed_sha256, observed_size}` only for a complete 2xx body with other bytes (a cut or failed download is `fetch_failed`), or `drift_seen` on a held entry; alert "the farm still serves the held copy; re-pin to a stable asset or vendor it"; an entry not held is retried with backoff and becomes `held` when a candidate serves the right bytes or a client's upload is adopted; the alert clears when the drifted URL leaves the entry's lists (re-pinning does that) or the entry leaves the report, and a drift note keeps it across a restart ([section 5.6](#56-the-fetcher)) |
| A wrong pin in a change | that change's build fails | the same check: nothing is stored; the CI wait step fails the change naming the entry | reported on the entry; no alert, because the change's own check is the signal |
| A pin bump hits many runners at once | N clients fetch upstream together | the list is submitted before the build; one copy writer per digest across fetch, adoption and healing ([section 5.2](#52-the-mirror-store-and-its-location)); a per-host concurrency cap; the first verified copy, fetched or adopted, serves everyone and later writers skip | in-flight count and queue depth per host; no alert unless a fetch fails |
| Cache poisoning | not applicable | the digest is the only key; a URL is a hint; every byte is verified before commit and on every read; nothing is keyed by URL; the action cache is never written | counter `mirror_rejected_total{reason}` |
| The fetcher used to reach internal addresses | not applicable | HTTPS only; a host allow-list by flag; no proxy; DNS resolved once per hop and the connection made to that address; every address that is not globally routable refused at every hop ([section 5.6](#56-the-fetcher)); the fetcher fetches only listed entries, never a URL given at miss time | refusal recorded on the entry; alert naming the refused URL and address and the fix, which is another URL: the filter has no override flag ([section 8](#8-report-alert-recover)) |
| A source's licence forbids a private copy | not applicable | a per-host policy: `fetch`, `adopt` or `deny` ([section 5.5](#55-per-host-policy)) | the report shows each entry's policy; a `deny` entry is reported, not alerted |
| Store pressure | not applicable | per-list caps, a total cap, and a separate cap for lists from unmerged changes inside the total, so those lists cannot crowd out the default branch ([section 7](#7-retention-and-caps)); over a cap, new entries are refused and held entries are never evicted | alert at 80% of a cap and on any refusal, naming the cap flag and the lists that hold the most; recover by raising it or retiring lists |
| The store damages a copy as it is written, or a write or its read-back fails | not applicable | every copy is read back and re-hashed before it is committed, for fetch and adoption alike; nothing points at an unconfirmed copy, so a digest in a segment stays served from there ([section 5.2](#52-the-mirror-store-and-its-location)) | report `store_write_failed{copy, error}`, the copy recorded as damaged and prunable; alert as a mirror-store write failure with the store error and the steps it calls for; retry with backoff as the next copy, from the spool for a fetch and by the sweep from the CAS for an adoption, clearing on commit |
| Server restart | cold CAS | held entries, the lists and the drift notes live under a stable prefix ([section 5.1](#51-fetch-lists), [section 5.2](#52-the-mirror-store-and-its-location)) and are read back at start, each copy reported present once its bytes are verified; every entry's state is derived again from the store, and the sweep then acts on every entry its state table takes ([section 5.4](#54-adopt-on-upload)) | re-index count, verified bytes and duration in the report; alert if the listing or a read fails, naming the store error; the re-index retries by itself, and no copy is written until its listing has succeeded |
| A damaged object in the store | not applicable | every mirror object is read and hashed at start before it is reported present ([section 5.3](#53-re-index-at-start)), so a damaged object found then is never served; damage after that is caught by the re-hash on read, which fails the actions that already passed Execute's input check ([section 5.3](#53-re-index-at-start) says exactly which); a copy damaged as it is written is caught by the read-back before it is committed ([section 5.2](#52-the-mirror-store-and-its-location)) | report `corrupt`; alert with the fix; heal by writing a new copy under a new key: the sweep copies the blob from the CAS if a segment holds it, or indexes an older copy of the store that still verifies, and otherwise the fetcher writes one under `fetch`; under `adopt` with neither, the alert names the one client build that heals it ([section 5.4](#54-adopt-on-upload), [appendix A](#appendix-a-entry-state-machine)) |

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
- **Lists survive a restart.** The index is in memory ([section 2.4](#24-today-a-restart-empties-the-cas)),
  so a list kept only there would be lost at every start, and with it every entry's
  adoption, fetch, sweep and grace until CI next submitted. So an accepted submission is
  written, before the `PUT` is answered, as one object
  `<mirror-prefix>lists/<name>/<generation>/<record>` (the list's entries, its expiry
  and the time it was accepted, followed by the SHA-256 of those bytes, so a start can
  check the record), with `put_new` and read back like a copy
  ([section 5.2](#52-the-mirror-store-and-its-location)); `DELETE` writes
  `<mirror-prefix>retired/<name>/<generation>/<record>` with its time the same way.
  `<record>` is numbered like a copy, so a damaged record is superseded, never
  rewritten. A write that fails refuses the request with the store error, and CI's step
  fails naming it ([section 6.1](#61-first-fetch-and-pin-bump)).
- **One writer per list name.** A `PUT` or `DELETE` holds its list name's lock from the
  generation check through the record write to the swap of the list in memory, so two
  requests for one name are applied one after the other, and memory always holds the
  generation of the newest record written. A `DELETE` writes the generation it retires;
  a later `PUT` for that name must carry a greater generation.
- **One cap check at a time.** The total and the unmerged caps span lists, so a lock per
  name would let two `PUT`s of different names each pass the total cap against a total
  that counts neither. So a `PUT`, once it holds its name's lock, also takes the
  server's one cap lock and holds it from the cap check through the record write to the
  swap; a `DELETE` and an expiry take it to evaluate the caps again
  ([section 7](#7-retention-and-caps)). Submissions are rare, so serializing their record
  writes costs little. The lock order is R1's
  ([section 5.2](#52-the-mirror-store-and-its-location)).
- **The list API waits for the start-time read.** Until the read of the list records
  below has ended, a `PUT` or `DELETE` cannot check its generation against the stored
  one or pick a record number, so it is answered `503` with `Retry-After`, and CI's step
  retries it within its bound ([section 6.1](#61-first-fetch-and-pin-bump)).
- **At start**, before the re-index of [section 5.3](#53-re-index-at-start) ends, the
  server reads, for each list name, its records under `lists/` and `retired/`. The newest
  verified record wins, by generation; for one generation a `retired` record wins over a
  `lists` record, since it retires that generation. A name whose winning record is
  `retired` names no live list. Expiry and grace are computed from the stored times. For
  grace ([section 3](#3-terms)) the server also reads the records that a newer
  generation replaced, or that were retired or expired, less than `--mirror-grace` ago,
  and takes each digest's grace from the time its last list stopped naming it (the
  acceptance time of the generation that dropped it, the retirement time or the
  expiry). A record that does not verify is reported and alerted under
  `unreadable_lists`, naming the list and the fix (resubmit it; a re-run of its CI job
  does), and the next older verified generation is used meanwhile; the resubmission is
  newer than that one, so it is accepted and written as the next record. A record that
  does not verify and is older than its name's winning record (a lower generation, or a
  lower record number of the same one) is not unreadable: it is superseded, and
  reported prunable like a damaged copy.
- **An unreadable name.** If no record of a name verifies, its entries cannot be known.
  Then every digest with a copy under `sha256/` that no verified record names is an
  *orphan*. With no URL, its host and so its policy are unknown:
  - If no `--mirror-host` flag names a `deny` host, an orphan is kept as an entry in
    grace with no URLs: it is served and healed from the CAS or an older copy, never
    fetched.
  - If some host is `deny`, an orphan may be that host's, which keeps no copy
    ([section 7](#7-retention-and-caps)), so it is not indexed and not served. Its copies
    are kept, not prunable, and it is counted under `unreadable_lists`.
  - Either way, an orphan's grace does not end while `unreadable_lists` is not empty.
    A resubmission that names an orphan makes it that list's entry, with its URLs and
    policy. The resubmission that empties `unreadable_lists` also lists, in its record,
    the orphans that no live list names (an `orphans` field). Their grace ends
    `--mirror-grace` after that record's acceptance time, like a digest a newer
    generation dropped, so a later start computes the same end; until then each keeps
    the treatment above.
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
- **R1, the copy lock.** Each digest has one copy lock for the whole server. **Every
  state change of an entry happens only while its copy lock is held**: a commit
  (`PutBlob`) of a new copy, the indexing of an existing copy, taking a copy out of the
  index, applying a change of the entry's *standing* (its URLs, policy and cap result,
  computed from the lists in memory: [section 5.5](#55-per-host-policy),
  [section 7](#7-retention-and-caps)), and recording its state. Under the lock the holder reads
  the entry's state and standing again, and commits or indexes only if they still allow
  it: the entry is live ([section 3](#3-terms)), not `held` (a committed copy that has
  not failed verification since), not `denied` and not `refused{over_cap}`; a fetch also
  commits or records only if the entry is still `fetching`, the fetch was not cancelled,
  and the entry's policy is still `fetch`. A writer that may not commit writes nothing:
  a fetch removes its spool file, an adoption is dropped and counted as
  `mirror_adopt_skipped_total`. Otherwise the writer takes the next copy number under
  the lock; a number taken in this start is never taken again, even if its write failed.
  So no check made outside the lock is relied on: a change of standing that arrives
  while a holder works waits for it and then acts on what it committed (R8 of
  [appendix A](#appendix-a-entry-state-machine)), and one that arrives first is seen by the
  holder's own check. The one change made without the lock is a serving read's damage
  mark ([section 5.3](#53-re-index-at-start)), which marks only the copy it read, so it
  cannot undo another copy's commit; from the mark on the entry is `corrupt{copy}`, and
  every holder sees it.
  - *Who waits.* A fetch (to commit, or to record how it ended), a verification (the
    re-index and the sweep's step 2) and a change of standing wait for the lock, and
    hold no slot and no bytes in memory while they wait (a fetch's bytes are in its
    spool file, read into memory only under the lock; a verifier takes its slot after
    the lock). The sweep and an adoption never wait. The sweep passes over an entry whose
    lock is taken, since the holder acts. An adoption from `store_blobs` holds the
    upload's bytes in an adopter slot; if the lock is taken it drops them, counts
    `mirror_adopt_deferred_total` and runs the sweep for the digest. No bytes are lost:
    the blob is in a segment, and the sweep re-reads it verified from the CAS.
  - *Lock order.* A name lock, then the cap lock ([section 5.1](#51-fetch-lists)), and a
    copy lock only with neither held: a `PUT` applies the changes of standing it causes
    after it has released them, and is answered once they are applied, so it can wait
    behind one copy write of up to the per-entry cap. A copy lock holder takes no other
    lock and waits for nothing but a verifier slot; the paths that take a slot first
    never wait. So there is no cycle.
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
    backoff: the entry goes back to the sweep ([section 5.4](#54-adopt-on-upload)),
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
  `--mirror-adopters` adoptions; nothing queues behind the slots, and a lock waiter
  holds no bytes, R1), plus
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
  with something else. The sweep ([section 5.4](#54-adopt-on-upload)) re-lists
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
  Each digest is verified under its copy lock (R1,
  [section 5.2](#52-the-mirror-store-and-its-location)). The same routine, run for one
  digest, is what a retried `unverified` read and the sweep's re-check of older copies
  (its step 2) run; they skip copies already recorded damaged in this start.
- When the re-index of a digest ends, and once for all entries when the whole re-index
  ends, the sweep of [section 5.4](#54-adopt-on-upload) runs for every entry that is not
  held, so a fetch, an adoption or a heal that a restart interrupted starts again
  without waiting for a client to ask.
- The REAPI listener does not wait for the re-index. Until a digest is verified it is
  absent, which is today's answer after a restart, so a client that asks early goes
  upstream as it does today and its upload is adopted. Verification reads at most the
  held bytes once (about 1 GiB today, [section 2.5](#25-scale)), `--mirror-verifiers`
  objects at a time (lean 8). The report shows its progress.
- A read that fails with a store error (not a mismatch) leaves the digest absent and
  marks it `unverified{error}`; it is retried with backoff and alerted after N attempts.
  It sets the read backoff and the read count of the sweep table
  ([section 5.4](#54-adopt-on-upload)), which say what the sweep does meanwhile: it
  adopts the blob if the CAS holds it, and wakes the fetcher only once N reads have
  failed or the copy is read and recorded damaged or `NotFound`; either writes a newer
  copy, which supersedes the unread one.
- A failed listing fails the re-index: the server reports it, raises an alert naming
  the store error, starts without the mirror rather than serving a partial view as
  complete, and retries the listing with backoff. Until a listing has succeeded every
  entry is `verifying`, and until a digest's own check ends no copy of it is written by
  any path (an upload is stored in a segment as today and left to the sweep that runs
  when the check ends), so a copy number is always chosen after a listing and never
  races the start-time check. A listing that succeeds but misses
  recent keys is not detectable as such; the missed copy is not indexed, so the entry
  is fetched or adopted again; on a `conditional_put` store that write meets
  `AlreadyExists`, which [section 5.2](#52-the-mirror-store-and-its-location) turns into
  "verify and adopt the existing object", and on another store it replaces the key with
  the same verified bytes.

**Damage after verification.** Bytes can still go bad after the start-time check. The
first read after that is `Cache::fetch`, which re-hashes, marks the object unreachable
and returns `UNAVAILABLE` (`crates/kbf-front/src/cache.rs`); it does the same when the
store answers `NotFound` or `InvalidRange` for the key (a copy removed from under kbf, or cut short). A read that fails
with any other store error marks nothing (`Cache::fetch` returns the error): the entry
stays `held`, that read fails, the next read tries again, and the failure is counted in
`mirror_read_failed_total` and alerted as a mirror-store read failure after N in a row.
If that first read is a
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
- The mark runs the sweep for that digest at once, which heals it without a client
  when it can, by the `corrupt` row of the sweep table
  ([section 5.4](#54-adopt-on-upload)): from the CAS, from an older copy still in the
  store (superseded, not yet collected), or by the fetcher under `fetch`. Under `adopt`
  with none of those, the next upload is adopted into the next copy; `FindMissingBlobs`
  answers "missing" for the digest from the mark on, so a client that needs it uploads
  it.
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

- When `Cache::store_blobs` commits a digest that a live entry names and the mirror
  store does not hold, and the entry is not `verifying`, `denied` or
  `refused{over_cap}`, the cache hands a copy of the verified bytes to a free adopter
  slot, at most `--mirror-adopters` at once (lean 4), to be written as the entry's next
  mirror copy. Nothing queues behind the slots: with every slot busy the adoption is
  dropped at once, holding nothing. The upload is acknowledged as today, after the
  segment commit; the copy is a background write. It is written, read back and
  re-hashed before `PutBlob` moves the digest to `Location::Mirror`, exactly as
  [section 5.2](#52-the-mirror-store-and-its-location) says for every copy, under the
  digest's copy lock (R1, which also says what an adoption that finds the lock taken
  does). Until then, and if the write fails, the digest keeps its segment location and
  is served from there.
- `store_blobs` commits only *fresh* blobs: a digest already present in the CAS is
  touched, not stored (`crates/kbf-front/src/cache.rs`). So once a blob is in a segment,
  `FindMissingBlobs` says "present" and no client uploads it again, and every adoption
  that did not commit (none was started because the list came after the upload, which is
  the common case under `adopt`, the default policy; the slots were busy; the lock was
  taken; or the write failed) must be finished by the farm. That is the *sweep*. It runs
  on every list submission and retirement, at the end of each digest's re-index and of
  the whole re-index ([section 5.3](#53-re-index-at-start)), for one digest when a read
  marks it damaged, an adoption of it is dropped or deferred (R7), a `FindMissingBlobs`
  miss names it ([section 5.7](#57-what-findmissingblobs-answers)) or one of its
  backoffs passes, and every
  `--mirror-sweep-interval` (lean 10 minutes). It visits every live entry (listed or in
  grace, [section 3](#3-terms)) and does, for each, what the sweep table below says. Each
  sweep also re-lists `<mirror-prefix>sha256/` (keys are few) only to refresh the
  report's `unparsed_keys`; it indexes nothing from that listing.

**The sweep table.** The sweep tries each entry's copy lock and passes over the entry
if it is taken (the holder acts, R1); otherwise it holds the lock while it runs the
entry's steps and records its state. It reads the entry's row in the state table, which
says whether the sweep takes it and which steps the state allows, and runs the first
allowed step whose condition holds and which no gate the entry carries holds back. If
that leaves no write or fetch started and no copy committed, because no step ran or
because the steps that ran ended with nothing (step 1's read failed, step 2 found no
copy that verifies), step 4 records the state. This table is the one statement of
what the sweep does and which steps can occur in each state; the rest of the document
cites it.

*Steps.*

1. **Adopt from the CAS.** Condition: the CAS holds the digest (`Present`; never a
   mirror copy, since the entry is not held). It takes an adopter slot, then reads the
   blob through `Cache::read_blob` (verified) and writes the copy
   ([section 5.2](#52-the-mirror-store-and-its-location)). No slot free: the entry is
   `adopt_pending` and the next sweep tries again. A read that fails
   (the segment was damaged or collected) goes on to step 2.
2. **Re-verify the store's copies.** Condition: the store has copies of the digest not
   recorded damaged in this start (an older copy of a `corrupt` entry, an unread copy of
   an `unverified` one, or a copy left unconfirmed by a failed write). It re-runs the
   start-time verification for that digest ([section 5.3](#53-re-index-at-start)) over
   those copies, highest first, and indexes the first that verifies: `held` (a fetch
   running for the digest then commits nothing, R1). A copy that does not match is
   recorded damaged, and the next lower copy is tried. A copy the store answers
   `NotFound` for is not a copy (a write that never landed, or a key removed): it is
   dropped from the entry's copies, and the next lower copy is tried. A copy whose read
   fails with any other store error is a *failed read*: it counts towards N and sets
   the read backoff, so step 4 records `unverified` unless a gate earlier in its order
   holds (a `fetching` entry stays `fetching`, by its row), and it ends the step, so
   the lower copies are not tried, exactly as at start
   ([section 5.3](#53-re-index-at-start)): a store fault is seldom one key's, and the
   read is retried after the backoff. When no copy verifies, for any of these reasons,
   the sweep goes on to step 3, where the gates still apply.
3. **Wake the fetcher.** Condition: the entry's policy is `fetch`
   ([section 5.5](#55-per-host-policy)). The fetcher retries from its spool file if it
   still has one. A fetch already running for the digest is joined, not doubled
   ([section 5.6](#56-the-fetcher)).
4. **Record why nothing ran**, each state with the alert of
   [section 8](#8-report-alert-recover) that names the fix:
   - If a gate held back a step whose condition held, the state that gate belongs to,
     taking the first in this order: the write backoff, `store_write_failed`; the read
     backoff or the read count, `unverified`; the fetch backoff, `fetch_failed` or
     `upstream_drift`, as the last fetch ended; a refused fetch, `refused`.
   - Otherwise no step's condition held, so the blob is not in the CAS, the store has no
     copy of it left to read, and its policy is `adopt`: no bytes are on the farm. The
     entry is `corrupt` if the store holds copies of it, all recorded damaged, and
     `waiting` if it holds none. So an entry in grace, which has a copy, is never
     `waiting`, and the state does not depend on whether a restart happened: a start's
     re-index finds the same damaged copies and records `corrupt` too.

*Gates.* A gate belongs to the entry, not to its state: a failure sets it, it is kept
through every change of state, and only what its row names lifts it. A restart lifts
every gate (R3). When a backoff passes it runs the sweep for that digest, so every retry
(a fetch, a write from the spool, a re-read, an adoption) is a step of this table and
obeys the entry's other gates: the fetcher's retry from its spool file is step 3.

| Gate | Set by | Holds back | Lifted when |
|---|---|---|---|
| write backoff | a failed write or read-back of a copy (`store_write_failed`) | steps 1 and 3 (both write a copy; step 2 writes nothing) | it passes; the next failure sets a longer one, up to the cap |
| fetch backoff | a failed or drifted fetch (`fetch_failed`, `upstream_drift`) | step 3 only, so an adoption from the CAS still runs | it passes, as above, or the entry's URLs change (R5) |
| read backoff | a failed read of a copy (`unverified`) | step 2 only | it passes, as above |
| read count | a failed read of a copy, while fewer than N reads of that copy have failed | step 3, whatever step 2 did, so a short store fault costs no egress | the Nth failed read; or that copy is read (it verifies, or is recorded damaged or `NotFound`); or a newer copy is committed |
| refused fetch | a fetch whose every candidate was refused by the host list, the address filter or the scheme (`refused`) | step 3: the same URLs would be refused again | the entry's URLs change in a resubmitted list (R5), or a start changes the flags |

*The state table.* "Sets" is the gate the state's own failure sets; the entry may carry
any gate an earlier state set as well, and the gates apply on top of this table.

| State | Taken | Steps allowed | Sets | Step 4 records ("the rule" is step 4's) |
|---|---|---|---|---|
| `held` | no: a committed copy that has not failed verification since | none | | |
| `verifying` | no: every entry while the re-index listing has not succeeded, and after that each entry whose own check has not ended ([section 5.3](#53-re-index-at-start): until then no path writes a copy of it or fetches it); the check's end runs the sweep for it | none | | |
| `denied` | no: no copy may be kept; when the entry's policy stops being `deny` (R8) it leaves this state and the sweep takes it | none | | |
| `refused{over_cap}` | no: no copy may be kept; the caps are evaluated again ([section 7](#7-retention-and-caps)), and when it fits it leaves this state | none | | |
| `fetching` | yes | 1, 2; 3 joins the running fetch | | `fetching`: while the fetch runs the entry stays `fetching` whatever a step records (a deferred or failed adoption, a failed read of step 2) or a gate holds back, keeping the gate; only a committed copy (`held`, and the fetch's commit then writes nothing, R1) or the fetch's end sets another state (R7) |
| `adopt_pending` | yes | 1, 2, 3 (step 1's condition holds unless the CAS lost the blob) | | by the rule: what the entry is once the CAS has lost the blob (`corrupt`, `waiting`, or the state of a gate it carries) |
| `waiting` | yes | 1, 2, 3 (by its meaning only step 1's condition can hold, when the blob reaches the CAS another way, until the entry's policy becomes `fetch`, R8) | | by the rule: `waiting` |
| `unverified` | yes | 1, 2, 3 | read backoff, read count | by the rule: `unverified` while either gate holds; under `fetch`, once the read count is lifted (the Nth failed read, or the copy read: recorded damaged or `NotFound`), step 3 runs |
| `fetch_failed`, `upstream_drift` | yes | 1, 2, 3 | fetch backoff | by the rule: its own state while the backoff runs; after it, step 3 runs |
| `store_write_failed` | yes | 1, 2, 3 | write backoff | by the rule: its own state while the backoff runs; after it, if the CAS lost the blob and the policy is `adopt`, `corrupt` (its copy was recorded damaged), `waiting`, or the state of another gate it carries |
| `refused{host_not_allowed, address_filtered, not_https}` | yes | 1, 2, 3 | refused fetch | by the rule: `refused` |
| `corrupt` | yes | 1, 2, 3 | | by the rule: `corrupt` under `adopt` with no bytes on the farm |

No step waits for a client's upload, which `FindMissingBlobs` would suppress once the
blob is present. Only an entry that step 4 leaves `waiting` or `corrupt` needs one, and
then the digest is absent, so `FindMissingBlobs` answers "missing" and a client that
needs the file uploads it.

- A dropped adoption counts `mirror_adopt_dropped_total` and runs the sweep for the
  digest; what a dropped, deferred or failed adoption leaves the entry in is R7 of
  [appendix A](#appendix-a-entry-state-machine). An entry that stays `adopt_pending` for
  longer than T is alerted, naming `--mirror-adopters`.
- `waiting` therefore means what it says: live, policy `adopt`, not in the CAS, and no
  copy of it in the store.
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

**An entry's URLs and policy.** One digest can be named by several live lists, with
other URLs, and one entry's URLs can name hosts with other policies. An entry's URLs are
the union of the URLs that the live lists' entries for the digest name (for an entry in
grace, the records that last named it, [section 3](#3-terms)), in the order the lists
were accepted, each once; but when some list without an expiry names the digest, only
the lists without an expiry count. For this rule a record in grace counts as its list
did: a default-branch record that last named an entry in grace is a list without an
expiry, so a change list that names that entry cannot change its URLs or policy. (The
change list does make the entry live again, so its grace is counted again from when
that list stops naming it, which can lengthen it.) Its labels are all of its lists'.
Its policy is the
most restrictive of its URLs' hosts' policies, `deny` over `adopt` over `fetch`, since
the bytes are the same whichever host serves them, and a licence that forbids a copy
forbids it for all. So an entry is fetched only when every one of its hosts is `fetch`.
A list from an unmerged change (one with an expiry) therefore cannot change the URLs or
the policy of an entry that a default-branch or release list names: a change that adds
a URL on a `deny` host to such an entry does not make its copies prunable, and its
merge, which puts the URL on a list without an expiry, does. The report shows every
live list's URLs for the entry, each with its host and policy, beside the entry's own
policy, and marks the URLs that do not count.

The URLs and the policy are evaluated again whenever the lists that name the digest
change (a submission, a retirement or an expiry, like the caps of
[section 7](#7-retention-and-caps)) and at every start, since `--mirror-host` is a
flag. A change is applied under the entry's copy lock, like every change of standing
(R1, [section 5.2](#52-the-mirror-store-and-its-location)); what it does to the
entry's state is R5 and R8 of [appendix A](#appendix-a-entry-state-machine).

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
  with what was observed; the remaining `urls[]` are still tried. When every candidate
  has been tried and none gave the right bytes, the entry is `upstream_drift` if any
  candidate returned a complete body of other bytes, `refused` if every candidate was
  refused, and `fetch_failed` otherwise, with each URL's own result in the report. That
  is the only place `upstream_drift` is recorded. An entry in `upstream_drift` that is
  not held is retried on the same capped backoff as `fetch_failed`, because a moved tag
  can move back and a candidate can be fixed.
- **Drift notes.** Each drift observation is also written, once, as a small object
  `<mirror-prefix>drift/<hex>/<size>/<n>` (`{url, observed_sha256, observed_size, at}`,
  numbered, write-once and checked like a list record), and read back at start with the lists
  ([section 5.1](#51-fetch-lists)), so its alert survives a restart. A note whose write
  fails is still reported and alerted in this start, marked "not stored", and its write
  is retried with backoff. A note is *live* while the entry is live and some live
  list's entry for the digest (for an entry in grace, a record that last named it,
  [section 3](#3-terms)) still names the note's URL, and no later note records a
  match at that URL. When the entry becomes `held`
  (a retry from another candidate matches, or a client's upload is adopted,
  [section 5.4](#54-adopt-on-upload)), its live notes are shown as the annotation
  `drift_seen{url, observed_sha256, observed_size, at}` with the re-pin alert. A held
  entry is never fetched again, so the annotation does not wait for a fetch: it clears
  when its URL leaves every live list's entry for the digest (the alert's own fix,
  re-pinning to another URL with the same bytes, does that) or the entry leaves the
  report. A retry that matches *at the URL that drifted* shows that the URL serves the
  right bytes again: the match is written as a note the same way, that URL's earlier
  notes stop being live at once, and the entry is `held` with no annotation from them.
- **Writing the copy.** When the spool matches, it is written as the entry's next copy
  (read into memory for `put_new`), read back and re-hashed, and only then committed,
  as [section 5.2](#52-the-mirror-store-and-its-location) says; a read-back that does
  not match, or a store error, is `store_write_failed`, retried from the spool, never
  `upstream_drift`. The commit, and the record of how a fetch ended, are made under the
  copy lock by R1: a fetch whose entry left the report, became `held` or `denied`, or
  whose policy left `fetch` while it ran writes and records nothing and removes its
  spool file; a fetch healing an entry that went into grace while it ran commits.
- **Candidates.** `urls[]` are tried in order; each is verified the same way.
- **Concurrency.** One fetch per digest at a time (later callers wait on it; a
  cancelled fetch that has not ended yet does not count), its commit under the copy
  lock (R1); at most
  `--mirror-host-concurrency` per host, at most `--mirror-fetchers` in all. A failure
  retries with capped exponential backoff.
- **OCI registries.** A blob URL names its digest, but a registry may want an anonymous
  bearer token even for a public image. v0 does not implement the token exchange; such
  hosts stay on `adopt` until it is built.

### 5.7 What `FindMissingBlobs` answers

- A held entry is in the index, so `Cache::find_missing` answers it from memory, as it
  answers any blob today. The new code adds no object-store call to that RPC and
  changes no answer: for a digest it reports missing it does one in-memory lookup in
  the entry map.
- A live entry that is not held is reported missing, as today, and the miss schedules
  the sweep for that digest in the background. It does nothing else: whether the
  fetcher is woken is decided by the sweep table of [section 5.4](#54-adopt-on-upload)
  alone, with its exclusions and gates, so a miss fetches nothing the sweep would not.
  v0 never holds the RPC while a fetch runs
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
   digests whose entry's policy ([section 5.5](#55-per-host-policy)) is `fetch`.
3. The CI step polls `GET /v1/mirror` until every entry of its list whose policy, as
   the report gives it, is `fetch` is held, for at most a bounded time, and fails
   naming each such entry that is not held, with its URL, state and last error. It
   reads the entry's policy, not the policy of its own URL's host: another list can
   make an entry on a `fetch` host `adopt`, and the farm never fetches it. A wrong pin
   on a `fetch` host therefore fails at submission time, not in whichever build meets
   it first. An entry whose policy is `adopt` or `deny` is never fetched, so the step
   does not wait for it: it prints each one with its state (`waiting`, `adopt_pending`,
   `denied`) and does not fail on it; the build after the step downloads an `adopt` entry and its upload is
   adopted, and a wrong pin there fails that build as it does today. A `503` that the
   list API answers while a starting server is still reading its list records
   ([section 5.1](#51-fetch-lists)) is retried within the same bound. If the farm
   cannot be reached, or refuses the submission for a reason
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
missing, as every blob is after a restart today. The lists come back from the store with
them ([section 5.1](#51-fetch-lists)), and when the re-index ends the sweep acts on every
entry that is not held, as its state table says ([section 5.4](#54-adopt-on-upload)): under `fetch` it wakes the
fetcher, with no client involved. A live digest that is still absent (its fetch failed,
or its policy is `adopt` and no client has uploaded it) is reported missing as today; a
`FindMissingBlobs` miss on it also runs the sweep for it, which under `fetch` finds
the fetcher already running, in its backoff, or held back by another of the entry's
gates (the read count, a refused fetch; [section 5.4](#54-adopt-on-upload)).

### 6.3 Upstream outage

Held entries are served with no upstream contact. Entries not yet held fail on the
client as they do today, and the report and alert name them and their host. Recovery
needs no person: the fetcher's retry succeeds when the host returns, or any client that
can reach the file uploads it and it is adopted.

### 6.4 Upstream drift

A held entry is never refetched, so drift is seen only when a fetch runs for an entry
that is not held: its first fetch, a retry, or a heal after `corrupt`. The scrub reads
the mirror store, not upstream, so it sees no drift. An entry held after drift, by a
fetch from another URL or by an adoption, is `held` with the `drift_seen` annotation
and the re-pin alert, which clear when the drifted URL leaves the entry's lists or the
entry leaves the report ([section 5.6](#56-the-fetcher)); the drift notes keep both
across a restart. Drift on an entry that is not held is retried with backoff; a cut or
failed download is never drift. An entry that is only adopted is never compared with
upstream at all, so drift on an `adopt` host goes unseen; the report says so per entry.

### 6.5 A file written to the client's disk

komira#563's third case. With buck2 after the probe of section 2.3, a probe hit
declares the file as a CAS artifact, so writing it locally reads from the CAS and a held
entry covers this case too (read from the source, not tested). The release komira pins
downloads from the URL. Closing the case is a client decision
([D6](#d6-local-materialization-and-the-build-tools-own-download)).

## 7. Retention and caps

- An entry is kept while any live list names it, and, if it has a copy, for
  `--mirror-grace` after the last list that named it is replaced, retired or expires
  (lean 90 days), so older commits and bisects still build after upstream changes.
  Whether it stays, and how it is served and healed, is [section 3](#3-terms)'s rule.
  The times come from the stored list records ([section 5.1](#51-fetch-lists)), so a
  restart neither shortens nor restarts the grace. Then it is reported as prunable and
  removed by the planned collector, never silently. Superseded and damaged copies
  ([section 5.2](#52-the-mirror-store-and-its-location)) are reported as prunable at
  once and removed the same way.
- Caps: per entry, per list and in total, all flags. Over a cap new entries are refused
  with a reason; a held entry is never evicted to make room. Caps are evaluated again on
  every submission, retirement and expiry and at every start, one evaluation at a time
  under the cap lock of [section 5.1](#51-fetch-lists), in the order the entries'
  lists were accepted, so a held entry keeps its place; an entry `refused{over_cap}`
  that now fits leaves that state and the sweep takes it. Each entry's result is a
  change of standing, applied under its copy lock (R1,
  [section 5.2](#52-the-mirror-store-and-its-location)).
- An entry whose policy becomes `deny` keeps no copy, whether a `--mirror-host` flag
  changed it at a start or a change of its lists did while the server runs
  ([section 5.5](#55-per-host-policy); R8 of
  [appendix A](#appendix-a-entry-state-machine)): the entry is `denied` (R8), nothing
  indexes or commits a copy of it (R1), and its copies are reported prunable, for the
  collector to remove after the report. A list from an unmerged change cannot do this
  to an entry that a list without an expiry names (section 5.5).
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
generations that name it (or, in grace, when its last list stopped naming it and when
the grace ends), its policy and each URL's host policy ([section 5.5](#55-per-host-policy)), its state, its copies (verified, superseded, damaged
or unconfirmed) and the last error. An entry has one state, set by the latest event;
every transition, and the automatic path from every state back to `held`, is in
[appendix A](#appendix-a-entry-state-machine). States:

| State | Meaning | Clears when |
|---|---|---|
| `held` | a verified copy is in the mirror store and indexed, and has not failed verification since; it may carry the annotation `drift_seen{url, observed_sha256, observed_size, at}` | the annotation, with its alert, clears when its URL leaves every live list's entry for the digest or the entry leaves the report ([section 5.6](#56-the-fetcher)); a retry that matches at the URL that drifted leaves no annotation from that URL |
| `verifying` | at start, the re-index listing has not yet succeeded, or this digest's copies are not yet read and hashed ([section 5.3](#53-re-index-at-start)); the sweep passes over it and nothing fetches or writes it | the check ends: `held`, `corrupt`, `unverified`, `denied` or `refused{over_cap}` (policy and caps at this start), or, with no copy, whatever the sweep finds |
| `unverified{error}` | a copy could not be read (a store error, not a mismatch), at start or when the sweep re-checks the store's copies | a retried read succeeds, or the sweep writes a newer copy (adopted at once if the CAS holds the blob, fetched under `fetch` only after N failed reads or once the copy is read and recorded damaged or `NotFound`: the read count of the sweep table, [section 5.4](#54-adopt-on-upload)) |
| `fetching` | a fetch is running | the fetch ends |
| `waiting` | live, policy `adopt`, not in the CAS, and no copy of it in the store, so never in grace | a client uploads it, or the sweep finds it in the CAS, and its adoption commits; or the policy becomes `fetch` |
| `adopt_pending` | the CAS holds it but the copy into the mirror has not been written yet: the sweep's step 1 found every adopter slot busy | the next sweep writes it; if the CAS lost the blob meanwhile, the sweep table's steps 2 to 4 take it on ([section 5.4](#54-adopt-on-upload)) |
| `fetch_failed{host, error, attempts, next_retry}` | the last fetch failed in transport, ended early, got a status other than 2xx, timed out or found the spool full; never a mismatch ([section 5.6](#56-the-fetcher)) | a retry succeeds, or the blob is adopted (a client's upload, or by the sweep if the CAS holds it) |
| `upstream_drift{url, observed_sha256, observed_size, attempts, next_retry}` | not held, and every candidate that returned a complete 2xx body returned other bytes | a retry from any candidate matches, or the blob is adopted, from a client's upload or by the sweep from the CAS (`held`, with `drift_seen` unless the match was at the URL that drifted), or the entry leaves the report |
| `store_write_failed{copy, error, attempts, next_retry}` | a copy was written but its read-back did not match or could not be read, or the write failed; nothing was committed ([section 5.2](#52-the-mirror-store-and-its-location)) | a retried write of the next copy is read back verified and committed: from the spool for a fetch, by the sweep from the CAS for an adoption; or the sweep indexes a copy of the store that verifies; if the CAS lost the blob, the sweep table's steps 2 to 4 take it on ([section 5.4](#54-adopt-on-upload)) |
| `refused{host_not_allowed, address_filtered, over_cap, not_https}` | the farm will not fetch it (`not_https` arises only at a redirect: a list with an `http` URL is refused whole at submission) | `host_not_allowed`: the `--mirror-host` flag changes; `over_cap`: the cap flag or the lists change; `address_filtered` and `not_https`: the entry's URLs change in a resubmitted list. Except for `over_cap`, the blob is also adopted (a client's upload, or by the sweep if the CAS holds it), which makes it `held`: adoption needs no fetch |
| `denied` | the entry's policy is `deny` ([section 5.5](#55-per-host-policy)) | the policy changes: a flag at a start, or a change of its lists while the server runs |
| `corrupt{copy}` | the copy the index named failed verification (on read, or in the scrub), or every copy the store holds of it is recorded damaged (at start, or when a write's read-back did not match), and no verified copy is committed | a verified copy is committed: adopted from the CAS, an older copy of the store re-verified, fetched, or adopted from a client's upload; this survives a restart, because re-index verifies before it indexes, finds the damaged copy damaged again and tries the next lower |

Totals: held bytes, caps and their use, entries per state, re-index count, verified
bytes and duration of this start, spool files removed at start, the superseded and
damaged copies awaiting removal, `unreadable_lists`, drift notes not yet stored, and the
time of the last sweep and the entries it acted on.

**Metrics.** `mirror_fetch_total{host, result}`, `mirror_bytes_fetched_total`,
`mirror_bytes_held`, `mirror_adopted_total`, `mirror_adopt_dropped_total`, `mirror_adopt_skipped_total`, `mirror_adopt_deferred_total`,
`mirror_client_fetched_total`, `mirror_rejected_total{reason}`, `mirror_corrupt_total`,
`mirror_read_failed_total`,
`mirror_hits_total` (present answers for listed digests). `kbf-server` has no metrics
endpoint today, so v0 puts these counters in the report.

**Alerts.** Each names the entry, the host and the exact fix:

| Alert | Fix it names |
|---|---|
| an entry of a default-branch list not held after T | under `fetch`: the URL and error; none needed if the host returns (the fetcher keeps retrying); if the URL is gone, add a working URL to the entry's `urls[]` in the client and resubmit the list, or vendor the file. `waiting` under `adopt`: one client build, with a fresh buck2 daemon, of the declaring target named by the entry's label ([section 9](#9-the-clients-side-komira)), whose upload is adopted; or `--mirror-host <host>=fetch`, if the host's terms allow a copy |
| `upstream_drift`, or `drift_seen` on a held entry | do not accept the new bytes; re-pin to a stable asset or vendor the file; the farm keeps serving a held copy and keeps retrying an entry that is not held |
| a host failing for longer than T | the host, the error and the entries at risk; the same fix as the row above: none if the host returns, otherwise another URL for those entries, or `--mirror-host <host>=adopt` and one client build that downloads them |
| `refused{host_not_allowed}` | `--mirror-host <host>=fetch` (or `adopt`) and the flag's current value; meanwhile a client's upload is adopted |
| `refused{address_filtered}` or `refused{not_https}` | the URL and the address or scheme refused; no flag allows it (the filter has no override, by design); add an `https` URL on a public address to the entry's `urls[]` and resubmit the list, or vendor the file; meanwhile a client's upload is adopted |
| a cap at 80%, or any refusal over a cap | the cap flag and its value, and the lists holding the most bytes, to retire with `DELETE /v1/mirror/lists/{name}` |
| re-index failed, a mirror-store read failed, or `store_write_failed` after N attempts | the store error, the key and the store flags (`--s3-endpoint`, `--s3-bucket`, `--s3-region`, `--s3-prefix`, `--s3-conditional-put`, `--mirror-prefix`, and the credential variables). The step to take follows the error: an authorization error, fix the credential the server runs with; a signature error that names the region, correct `--s3-region`; a store that refuses the conditional write, remove `--s3-conditional-put`; a missing bucket or a refused prefix, correct the flag; a store that is down or out of space, restore it. A read-back mismatch means the store returned other bytes than it was given: check the store's own health and disks, since no fix in kbf applies. Every retry is automatic and the state clears when one succeeds |
| `unverified` after N attempts | the store error and the key, with the same steps as the row above; meanwhile the digest is absent, and the sweep writes a newer copy, which supersedes the unread one: from the CAS if a client has uploaded it, or, under `fetch`, from the fetcher |
| `unreadable_lists`, or a drift note not stored after N attempts | the list or note key and the store error, with the same steps as the store row; for a list, resubmit it (a re-run of its CI job does); a note's write is retried by itself |
| `unparsed_keys` | the keys; remove them from the store, or set `--mirror-prefix` to a prefix used by nothing else |
| `adopt_pending` for longer than T | `--mirror-adopters` (nothing queues behind its slots) |
| `corrupt` | the digest and the damaged key; none if the CAS holds the blob or an older copy still verifies (the sweep heals it at once); under `fetch` none (the fetcher writes a new copy); under `adopt`, otherwise, one client build, with a fresh buck2 daemon (`buck2 kill` first, since a running daemon may have cached "present"), of a target whose remote actions take the file as input; the alert names the declaring target from the entry's label ([section 9](#9-the-clients-side-komira)), and the build's upload is adopted. Or `--mirror-host <host>=fetch`, if the host's terms allow a copy |
| spool full | the spool directory, the bytes free and the bytes needed |

`kbf-alert` has no code yet (`crates/kbf-alert/src/lib.rs` is a module comment), and
`kbf-server` delivers no alerts. v0 exposes every alert state in the report and logs it
once when it appears and once when it clears; delivery to the operator is
komira-ai/komira-build-farm#189 (native alerting, which covers "the other planned
alerts" as well as node attention items), and these alerts go through the same path.

**Recover.** Every sweep ([section 5.4](#54-adopt-on-upload)) moves every live entry
that its table takes toward `held` by the first path that needs no person and that no
gate holds back: the CAS's bytes, an older copy that still verifies, or the fetcher. So
every state but four clears by itself once its cause clears, and none of those paths
waits for a client upload, which `FindMissingBlobs` would suppress once the blob is
present.
The four are deliberate states for a person, each with an alert naming the fix:
`waiting` and `corrupt` under `adopt` with no bytes on the farm (one client build;
`FindMissingBlobs` answers "missing" for the digest, so that build uploads it),
`refused` (a URL or a flag), and `denied` (a policy). A flag fix takes effect at the
next start, after which the sweep acts; nothing else needs a restart or a command
beyond the fix the alert names. [Appendix A](#appendix-a-entry-state-machine) checks
this state by state and event by event.

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
  client daemon and no new CLI: the step is an HTTP request and a poll. It waits only for
  entries whose policy in the report is `fetch`. Its test: a list whose `fetch` entries
  are held and whose `adopt` entry is `waiting` passes; one with a `fetch` entry in
  `fetch_failed` fails naming it; and one whose entry is on a `fetch` host but `adopt`
  because another list names it passes. Mutants: wait for every entry (the first case
  times out); wait by the entry's own URL's host (the last case times out).
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
  nothing; with other entries it is refused. Mutant: accept any equal generation. Two
  `PUT`s of one name, generations 5 and 6, released together with the store holding the
  first record write: memory and the store both end at 6. Mutant: no lock per list name
  (memory can end at 5). A start with `retired/x/5` and `lists/x/6` serves generation 6;
  with `lists/x/5` and `retired/x/5` it serves none. Mutant: prefer `lists/` over
  `retired/` (the second case serves 5). A start where no record of a list verifies keeps
  every unnamed digest with a copy in grace, served, never fetched, and its grace does
  not end while the list is unreadable. Mutant: drop the digests of an unreadable list
  (an orphaned held entry is not served). The same start with `--mirror-host x=deny`
  set serves no orphan and reports none prunable. Mutant: index orphans whatever the
  flags (an orphan answers present). A resubmission that names one of two orphans
  makes it an entry with its URLs; the other's grace ends `--mirror-grace` after the
  resubmission, and after a restart at the same end. Mutant: no `orphans` field (after
  the restart the other is not in grace at all, so its copy is not served).
  A `PUT` sent during the start-time read of the records is answered `503`, and the
  same `PUT` after it is checked against the stored generation. Mutant: answer before
  the read (an older generation than the stored one is accepted).
- Caps: change lists fill the unmerged cap, and a default-branch list is still
  accepted. Mutant: charge every list to the total only. Two `PUT`s of different
  names, each within the total cap alone but not together, released together with the
  store holding the first record write: exactly one is accepted. Mutant: no cap lock
  (both are accepted and the total is over its cap).
- Entry policy: an entry with one URL on a `fetch` host and one on an `adopt` host,
  and a digest named by two lists without an expiry whose URLs are on those two hosts,
  are each `adopt`, never fetched, and report both URLs. Mutant: take the first URL's
  host (the fake upstream sees a request).
- Policy while the server runs: a held entry of a default-branch list, then a second
  list without an expiry naming its digest with a URL on a `deny` host: the entry is
  `denied` at once, `FindMissingBlobs` answers "missing", and its copy is reported
  prunable; that list's retirement makes it `held` again on the same copy, re-verified,
  with no write. The same second list with an expiry leaves the entry `held`, not
  prunable, and the report marks the `deny` URL as not counting. A `waiting` entry whose
  only `adopt`-host URL leaves its lists, the rest on `fetch` hosts, is fetched with no
  restart; a `fetching` entry whose list adds a URL on an `adopt` host has its fetch
  cancelled, and the fetch writes nothing. Mutants: evaluate policy only at a start (the
  first case stays `held` and answers present); let a list with an expiry set the policy
  of an entry a list without one names (the second case is `denied`); check the policy
  only when a fetch starts (the cancelled fetch commits). The race of a change with a
  lock holder (R1): a `corrupt` entry whose step-2 re-verification of an older copy is
  stalled in the fake store, and a fetch stalled under the copy lock after its read-back
  and before its `PutBlob`; in each window a list arrives that makes the entry's policy
  `deny`. The `PUT` is answered only after the holder releases the lock, and then the
  entry is `denied`, `FindMissingBlobs` answers "missing" and no copy is indexed.
  Mutant: apply the change without the copy lock (the re-verified copy is indexed, or
  the fetch commits, and the digest answers present). A default-branch entry in grace (a
  newer generation dropped its digest), then a change list naming it with a URL on a
  `deny` host: it stays `held`, not prunable. Mutant: count a record in grace as a list
  with an expiry (the entry is `denied`).
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
  listed as damaged or prunable; when the adoption finds the lock taken it is counted
  in `mirror_adopt_deferred_total` and the next sweep writes nothing. Mutant: let an
  adoption that finds the lock taken wait with its bytes (the deferred counter stays at
  0, so that assertion goes red). Mutant: take the copy lock in the fetcher only (both
  write the same number, the second overwrites the committed copy, and its read-back
  marks it prunable, so both assertions go red).
- Heal after damage on read: copy 0 is committed, then damaged in the store, then read
  through `Cache::fetch`, so the entry is `corrupt{0}` while `PutBlob` still names copy
  0. Under `fetch` the woken fetcher, and under `adopt` a client upload, each take the
  copy lock, do not skip, write copy 1, read it back and commit it; the entry is `held`
  and the state clears, and `mirror_adopt_skipped_total` does not move. Mutant: skip on
  any committed copy (the writer sees copy 0 committed and writes nothing, so `corrupt`
  never clears and the state assertion goes red). Under `adopt`, extended: the client's
  upload heals the index into a segment (the blob is `Present`), and the store damages
  the heal write of copy 1, so the entry is `store_write_failed` and no client uploads
  again. After the backoff the next sweep takes the entry, reads the blob from the CAS,
  writes copy 2, reads it back and commits it, and the state clears. Mutant: the sweep
  selects only entries with no committed copy (it passes over the entry, since copy 0
  was committed, so the state never clears). Under `fetch`, extended: while the woken
  fetch runs, the last list naming the digest is retired; the fetch's copy is committed
  and the entry is `held`, in grace, with its alert cleared. The same with the list
  retired while the entry is `unverified`, and while it is `adopt_pending` after the
  damage: each stays in grace and ends `held`. Mutant: judge grace by the entry's
  current state, as "held or `corrupt`" (the fetching entry leaves, its commit writes
  nothing, and the `held` assertion goes red).
- Sweep selection: one live entry in each state that is not `held`, each with the blob
  in a segment of the CAS: every one but `verifying`, `denied`, `refused{over_cap}`, an entry whose
  copy lock is held, and a `store_write_failed` entry before its retry time is adopted
  by one sweep, a `fetch_failed` entry in its fetch backoff included; a
  `corrupt` entry with no CAS copy and a good older copy in the store is healed by
  indexing that copy, with no write and no upload, and so is a `store_write_failed`
  entry that was healing a `corrupt` one and whose CAS copy is gone. A `verifying`
  entry, after the listing has succeeded and before its own check has taken its lock,
  with the blob in a segment, on a `fetch` host, is passed over by a sweep that a list
  submission starts: no copy is written and the fake upstream sees no request; the
  check then indexes its good copy with no write. Under `adopt`, an entry that was
  healing a `corrupt` one and is left with no bytes on the farm is recorded `corrupt`,
  not `waiting`. Mutants: skip `corrupt` (its case goes red); let a fetch backoff hold
  back the adoption (the `fetch_failed` case stays unheld); skip the older-copy step
  (under `fetch` a copy is fetched and written, so the no-write assertion goes red;
  under `adopt` the entry stays `corrupt`); run the older-copy step only for `corrupt`
  and `unverified` (the `store_write_failed` case writes or stays unheld); omit the
  `verifying` exclusion (a copy is written, or a fetch started, during the check, so
  the no-write and no-request assertions go red).
- Sweep gates: the sweep table's gates hold back steps, not states. An `unverified`
  entry on a `fetch` host, after the first failed read of its copy 3, in its read
  backoff, with no CAS copy: a sweep started by a `FindMissingBlobs` miss sends no
  request to the fake upstream and leaves it `unverified`; so does a sweep after the
  backoff whose re-read of the copy fails with a store error again (attempt 2, below N);
  the sweep after the Nth failed read wakes the fetcher, which writes copy 4 and the
  entry is `held`. The same entry after a dropped adoption (`adopt_pending`, then the
  CAS loses the blob) still sends no request before N. A `fetch_failed` entry whose
  adoption failed (`store_write_failed`) and whose CAS copy is then lost is recorded
  `store_write_failed` while its write backoff runs and `fetch_failed` after it, while
  its fetch backoff runs, never `adopt_pending` or `waiting`. A `fetching` entry (its
  copy 3 unread after N failed reads, so the fetcher was woken) whose fake upstream
  stalls while copy 3's read backoff passes: when the re-read of copy 3 verifies, the
  entry is `held` on copy 3 and the fetch's commit writes nothing; when it fails again,
  the entry stays `fetching`, and the fetch's end sets its state. A `store_write_failed`
  entry whose highest copy's re-read fails with a store error, with a good lower copy,
  is not indexed on the lower copy by that sweep, and is `store_write_failed` while the
  write backoff runs. Mutants: the before-N
  exclusion only in step 2 (the miss's sweep fetches, so the no-request assertion goes
  red); treat a store error on a step-2 re-read as "none verifies" with no count (the
  count never reaches N, so the `held` assertion goes red); tie the read count to the `unverified` state (the
  `adopt_pending` case fetches); step 4 keeps the current state (the `fetch_failed`
  case is `store_write_failed` after its backoff, or `adopt_pending`); let step 2's
  failed read set `unverified` during a fetch (the stalled-fetch case leaves
  `fetching`); let step 2 go on to a lower copy after a failed read (the lower copy is
  indexed in that sweep).
- No bytes on the farm: on an `adopt` host, an entry whose first adoption's `put_new`
  succeeded but whose read-back did not match, and whose CAS copy is then lost, is
  `corrupt` and alerted, before and after a restart, and when its list is retired it
  stays in grace, still `corrupt`. An entry whose write never landed (the re-read is
  `NotFound`) is `waiting`. Mutant: step 4 records `waiting` whenever no committed copy
  failed verification (the first case is `waiting` before the restart and `corrupt`
  after it).
- Adopter slots: with every adopter slot held by a stalled write, 100 `store_blobs`
  adoptions are dropped, each counted in `mirror_adopt_dropped_total`, and the bytes the
  mirror holds stay at most `--mirror-adopters` blobs (the fake store records each body
  it is handed, and the test counts the live `Bytes` the adoptions keep). Mutant: queue
  adoptions with their bytes behind the slots (the live count exceeds the bound).
- Lock with a retried read: a digest is `unverified` on copy 3, an upload's adoption
  commits copy 4, and the retried read of copy 3 then succeeds; the index names copy 4
  afterwards and copy 3 is superseded. Mutant: retry and index without the copy lock
  (the index names copy 3). An upload during the start-time check of its digest, whose
  copy 3 is good, starts no adoption, and the check indexes copy 3 with no write.
  Mutant: adopt during the check (a copy is written that was not needed, so the no-write
  assertion goes red).
- Drift notes: an entry drifts at URL A and is held through URL B, so it is `held` with
  `drift_seen`; a restart keeps the annotation; a resubmitted list whose entry names B
  only clears it with no fetch. Mutants: keep the note only in memory (the restart
  drops the alert); clear the annotation only on a fetch of A (it never clears, since a
  held entry is never fetched).
- Policy: an entry whose policy is `adopt` is never fetched; one whose policy is `deny`
  is never adopted. Mutant: `adopt` fetches.

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
   mirror with no upload. Every adopter slot busy: `adopt_pending`, then held after the
   next sweep. Mutants: adopt only from `store_blobs`; clear the entry on a dropped copy.
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
11. A list naming one entry on a `fetch` host whose upstream is down, then a restart,
   then the upstream up: with no client request, the entry is `held` within one sweep
   interval, the list's generation is the one submitted, and an equal-generation
   resubmission with other entries is still refused. Mutants: keep the lists only in
   memory (after the restart nothing names the entry); run no sweep at the end of the
   re-index and wake the fetcher only from `FindMissingBlobs` (the entry is not held
   until a client asks).

**Simulation** (a new family in `kbf-sim`, on the virtual clock, in the conventions of
[simulation.md](simulation.md)): upstream up, down and drifting at random; list
generations and client uploads interleaved, some of them changing an entry's policy
(to and from `deny`, and between `adopt` and `fetch`) while a writer or a verifier
holds its copy lock; restarts; mirror copies damaged in the
store, copies damaged as they are written, listings that miss recent keys, upstream
responses cut short or answered with an error status, and fetch, adoption and healing of
one digest released at the same instant, run on a store with `conditional_put` and on
one without. After every step: a committed mirror copy
hashes to its key unless it was damaged after its last verification; no committed copy
is overwritten, or reported damaged or prunable, except after a failed verification of
that copy; after a restart no
copy is answered present before it is verified; at most one fetch per digest that is
not cancelled is in flight, and at most one copy lock holder per digest; no copy is
written and no fetch started
for a `verifying` entry, and no fetch is started while the entry carries a read count
below N, whatever its state; a fetch commits or records only while its entry is
`fetching` under `fetch` and the fetch is not cancelled; no entry whose policy is `deny`
has an indexed copy once the change has been applied (R1); no cut or error response is
recorded as
`upstream_drift`; no entry that has a copy is
lost while listed or in grace, whatever its state when its last list went; every listed entry that is not `held` carries a reason;
after every sweep, every live entry that is not `held` and not in one of the person
states of [section 8](#8-report-alert-recover) has a writer running, a backoff pending
or an adoption due at the next sweep; nothing outside the lists is fetched. At the end:
every state whose cause healed has cleared. Mutants: the sweep selects only entries with
no committed copy; apply a change of policy without the copy lock (a `deny` entry keeps
an indexed copy); the sweep omits the `verifying` exclusion; the before-N exclusion
only in step 2 (a fetch starts for an `unverified` entry in its read backoff); judge
grace by the
entry's current state; keep the lists only in memory (after a restart no entry is acted on,
so the sweep check goes red); drop single-flight; drop the re-index; let a failed fetch clear the
entry; index a copy before verifying it; commit a copy without reading it back; take
the copy lock in the fetcher only (on the store without `conditional_put` an adoption
overwrites a committed copy); skip on any committed copy (a copy damaged after its
commit is never healed, so the end check goes red); record an early EOF as drift.

**End to end**, on a deployed farm, in the shape of komira#563's proof:
1. A cold farm and a client whose upstream for one pin is unreachable: the build fails.
2. Submit the list with the true URL; the fetcher holds the entry; the same build passes
   with every action remote and no upstream contact.
3. Restart the server; the build passes again with no upstream contact.

## 12. Order of work

**v0:** the list API with validation, caps and stored list records; `Location::Mirror`,
the mirror store with numbered copies and the verifying re-index; adopt on upload and
the sweep; drift notes;
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

## Appendix A. Entry state machine

This appendix checks the states of [section 8](#8-report-alert-recover) against every
event, so that no state is left with no way back to `held`. Sections 4, 5 and 8 say the
same in prose; where they and this table disagree, that is a defect in the document.

**Events.**

| Event | What it is |
|---|---|
| fetch ok / fail / drift / refused | a fetch of the entry ends: right bytes in the spool; a transient failure on every candidate; other bytes on some candidate and the right bytes on none; every candidate refused by the host list, the address filter or the scheme ([section 5.6](#56-the-fetcher)) |
| upload | `store_blobs` commits the digest fresh into a segment ([section 5.4](#54-adopt-on-upload)) |
| adopt ok / deferred / fail | an adoption started by `store_blobs` commits a verified copy; finds every adopter slot busy or the copy lock taken and drops its bytes; or its write or read-back fails. An adoption the sweep starts is its step 1, part of the event "sweep" |
| sweep | the sweep of [section 5.4](#54-adopt-on-upload) acts on the entry: its steps 1 to 4 (adopt from the CAS, re-verify the store's copies, wake the fetcher, record why nothing ran) |
| read damage | `Cache::fetch` finds the mirror copy the index names wrong, missing or short, and marks it ([section 5.3](#53-re-index-at-start)) |
| store error | the object store fails a write, a serving read, a start-time verification read or the listing (a failed read in the sweep's step 2 is part of the event "sweep") |
| re-index | the start-time verification of this digest ([section 5.3](#53-re-index-at-start)); the same routine run for one digest later is the sweep's step 2 |
| scrub | later work ([section 12](#12-order-of-work)): a re-read of a held copy by the farm, with the outcomes of a read |
| list removed | no live list names the digest any more (replaced, retired or expired) |
| pin bump | a newer generation of a list names the digest, maybe with other URLs or another label |
| policy change | the entry's policy ([section 5.5](#55-per-host-policy)) changes: a `--mirror-host` flag at a start, or a submission, retirement or expiry while the server runs |
| restart | the server stops and starts |

**Rules every row follows.**

- R1. The copy lock ([section 5.2](#52-the-mirror-store-and-its-location)): every
  state change of an entry is made under it, and its holder checks the entry's state
  and standing again before it commits, indexes or records. Every row below obeys it.
- R2. An entry has one state, set by the latest event; R7 and the `fetching` row of the
  state table ([section 5.4](#54-adopt-on-upload)) name the exceptions. Its copies,
  damaged ones included, are listed beside it.
- R3. Restart: in-memory state (attempts, backoffs, locks, spool files) is gone. The
  lists, the drift notes and the copies are read back from the store
  ([section 5.1](#51-fetch-lists), [section 5.6](#56-the-fetcher),
  [section 5.3](#53-re-index-at-start)), and grace is rebuilt from the list records and
  the copies in the store ([section 3](#3-terms)); every entry is `verifying` until its
  digest is verified, and then the sweep acts on it. So the row "restart" is the same
  for every state: `verifying`, then whatever the store and the sweep find.
- R4. List removed: an entry that has a copy stays in grace in every state but
  `denied`, which leaves ([section 3](#3-terms)); an entry with no copy leaves the
  report with its alerts, and a writer running for it writes nothing (R1).
- R5. Pin bump: the entry takes the new URLs and label ([section 5.5](#55-per-host-policy)).
  If its URLs changed, its fetch backoff and a refusal of its last fetch are reset (a
  write or read backoff is not: it paces the store, not upstream), its policy is
  evaluated again (R8), and the sweep acts on it at once; a `drift_seen` whose URL is
  gone clears.
- R6. The sweep is the one of [section 5.4](#54-adopt-on-upload): when it runs, which
  states it takes, its four steps and their outcomes, and the gates that hold a step
  back whatever the entry's state is now. **Which steps can run in a state is that
  section's state table and nothing else**: each table below has one "sweep" row,
  which cites it and adds only what is particular to the state.
- R7. Adoption outcome: an adoption that fails records `store_write_failed`; one that
  is dropped (no adopter slot) or deferred (the copy lock taken) records nothing and
  runs the sweep for the digest (R1), whose step 1 records `adopt_pending` if no slot
  is free. While a fetch runs, every outcome but a commit leaves the entry `fetching`,
  and so does a failed read of the sweep's step 2 (the state table's `fetching` row).
- R8. Policy change ([section 5.5](#55-per-host-policy)): a change of standing, so it
  waits for the entry's copy lock and is applied under it (R1), in every state but
  `verifying`, whose check applies the policy in force when it ends. Holding the lock:
  - To `deny`: the indexed copy, if any, is marked unreachable as a damage mark marks
    it ([section 5.3](#53-re-index-at-start)), so `FindMissingBlobs` answers "missing",
    a later Execute is refused `MISSING`, and an action that already passed Execute's
    input check fails `INTERNAL` when it reads the blob; a client's re-upload is stored
    in a segment like any input and not adopted. A running fetch is cancelled. The entry
    is `denied`, and its copies are reported prunable
    ([section 7](#7-retention-and-caps)).
  - Any other change: a running fetch is cancelled if the policy left `fetch`, and the
    sweep's steps run for the entry under the same lock, so its record sets the state;
    from `deny`, step 2 re-verifies the copies the store still holds, which are no
    longer prunable.
- R9. Two events need something only some states have, and the tables do not repeat
  it. A read damage or a scrub needs an indexed copy, so it occurs only in `held`. A
  fetch's end changes the state only in `fetching` (step 3 makes an entry `fetching`
  and the `fetching` row keeps it there until the fetch ends); in any other state it
  writes and records nothing (R1).

**Transitions.** "Who" is the part of the server that acts. An alert in the alert
column is the [section 8](#8-report-alert-recover) row of that name. Every table's
"sweep" row has the outcomes the sweep's steps give, in the order the state table
allows them (R6): a committed copy is `held`; an adoption that does not commit is
R7; a fetch started is `fetching`; otherwise step 4's record.

`verifying`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| re-index: a copy verifies | `held` | verifier | none | not needed |
| re-index: every copy damaged | `corrupt` | verifier | `corrupt` | the sweep (R6) |
| re-index: no copy in the store | what the sweep at the end of the check records (R6) | sweep | the "not held after T" row, if it lasts | the sweep (R6) |
| re-index: policy `deny`, or over a cap at this start ([section 7](#7-retention-and-caps)) | `denied` (copies not indexed, prunable) or `refused{over_cap}` | verifier | `denied`: none; `over_cap`: the cap row | a person's decision; the caps evaluated again |
| store error on a copy's read | `unverified`, with the read backoff and the read count (R6) | verifier | `unverified` after N | the sweep, by the state table's `unverified` row |
| store error on the listing | `verifying` | start | re-index failed | listing retried with backoff; no copy written meanwhile |
| upload | `verifying`; the blob is in a segment, and no adoption starts for a digest still being checked | `store_blobs` | none | the sweep at the end of the digest's check |
| sweep | the state table's `verifying` row (R6); a `FindMissingBlobs` miss only runs the sweep | | | |
| policy change | `verifying`; the check's end applies the policy then in force | | | |
| list removed, pin bump, restart | R4, R5, R3 | | | |

`held`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| upload | `held` (not fresh: touched, not stored) | `store_blobs` | none | not needed |
| adopt or fetch commit arriving late | `held` (R1: it writes nothing) | writer | none; `mirror_adopt_skipped_total` | not needed |
| read damage | `corrupt{copy}` | serving read | `corrupt` | the mark runs the sweep for the digest at once (R6) |
| scrub finds damage | `corrupt{copy}` | scrub | `corrupt` | the same |
| store error on a serving or scrub read | `held` (nothing is marked); that read fails | serving read, scrub | mirror-store read failed, after N in a row | the next read; nothing to heal |
| list removed | `held`, in grace; at the end of grace, prunable | list API | none | not needed |
| pin bump | `held`; `drift_seen` clears if its URL is gone (R5) | list API | `drift_seen` clears | not needed |
| policy change (R8) | to `deny`: `denied`, the copy marked unreachable and the copies prunable; otherwise `held` | re-index, list API | none | to `deny`: a person's decision ([section 7](#7-retention-and-caps)) |
| sweep | the state table's `held` row (R6) | | | |
| restart | R3 | | | |

`fetching`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| fetch ok | copy written (R1): `held`, or `store_write_failed`; a match at a URL that drifted leaves no `drift_seen` from that URL | fetcher | `store_write_failed` after N | retry from the spool with backoff |
| fetch fail | `fetch_failed` | fetcher | "not held after T", host failing after T | retry with backoff |
| fetch drift | `upstream_drift` | fetcher | `upstream_drift` | retry with backoff; adoption of an upload |
| fetch refused | `refused{...}` | fetcher | `refused` | adoption of an upload; otherwise the URL fix (a person) |
| upload | adoption runs (the lock is free until the fetch commits): `held`, and the fetch's commit then writes nothing | adopter | none | not needed |
| adopt deferred or fail | `fetching` (the fetch goes on, R7) | adopter | none | the fetch, then the sweep |
| sweep | the state table's `fetching` row (R6): a copy that step 1 or 2 commits makes it `held`, and the fetch's commit then writes nothing (R1); any other outcome leaves it `fetching`, with the gates it set (a failed read of step 2 sets the read backoff and counts towards N) | sweep | none | the fetch |
| list removed | R4: with a copy (healing), `fetching` in grace, and the fetch commits; with none, the entry leaves and the fetch writes nothing (R1) | fetcher | with none, its alerts clear | the fetch |
| pin bump | `fetching`; the next attempt uses the new URLs | list API | none | not needed |
| policy change | R8: to `deny`, `denied`; to `adopt`, the fetch is cancelled and the sweep records the state | list API, start | per the next state | per the next state |
| restart | R3: the fetch and its spool file are gone; the sweep acts again | sweep | as before | the sweep |

`waiting` (policy `adopt`, the CAS does not hold it, the store has no copy)

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| upload | adoption: `held`; otherwise R7 | adopter | per the next state | the sweep |
| sweep | the state table's `waiting` row (R6): an adoption when the blob reached the CAS some other way, as upload; otherwise `waiting` | sweep | none | the sweep |
| policy change | R8: to `fetch`, the sweep at once, by the state table's row; to `deny`, `denied` | list API, start | per the next state | the fetcher, under `fetch` |
| no event | `waiting` | | "not held after T" (default-branch list), naming one client build or `--mirror-host <host>=fetch` | none: a person state. `FindMissingBlobs` answers "missing", so the build the alert names uploads it |
| list removed, pin bump, restart | R4, R5, R3 | | | |

`adopt_pending` (the CAS holds it in a segment)

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| sweep | the state table's `adopt_pending` row (R6): `held`; `adopt_pending` again if no slot is free; `store_write_failed` if the write fails; if the CAS read fails (segment damaged or collected), what steps 2 to 4 give | sweep | `adopt_pending` after T; `store_write_failed` after N; otherwise per the next state | the next sweep; per the next state |
| upload | `adopt_pending` (not fresh: touched; `store_blobs` starts no adoption) | `store_blobs` | none | the sweep |
| restart | R3: the CAS is empty after a start ([section 2.4](#24-today-a-restart-empties-the-cas)), so whatever the re-index and the sweep find | sweep | per the next state | per the next state |
| list removed, pin bump, policy change | R4, R5, R8 | | | |

`fetch_failed` and `upstream_drift`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| sweep, the fetch backoff passing included | the state table's `fetch_failed`, `upstream_drift` row (R6); an adoption from `upstream_drift` keeps `drift_seen` | sweep, fetcher | per the next state | the fetcher's retry |
| upload | adoption: `held` (with `drift_seen` from `upstream_drift`) | adopter | `drift_seen` stays until its URL leaves the lists (R5) or the entry leaves the report (R4) | not needed |
| adopt deferred or fail | R7; the fetch backoff is kept | adopter | per the next state | the sweep, by the next state's row |
| pin bump with other URLs | R5: the fetch backoff is reset and the sweep acts at once | sweep | the old alert clears if the new URL matches | the sweep |
| list removed | in grace if the entry has a copy (it was healing), and the retries go on; otherwise leaves (R4) | list API | clears if it leaves | the sweep, in grace |
| policy change, restart | R8, R3 | | | |

`store_write_failed{copy}`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| sweep, the write backoff passing included | the state table's `store_write_failed` row (R6). Particular to this state: for a fetch, step 3 writes the next copy from the spool file while it still hashes (R1: `held` or `store_write_failed`), and refetches (`fetching`) when it is gone or damaged; a copy its read-back recorded damaged counts for step 4 (`corrupt`), and one it left unconfirmed is one step 2 reads | sweep, fetcher | after N, mirror-store write failure with the store steps | the next retry |
| upload | not fresh if the CAS holds it (the sweep acts); fresh otherwise, and adopted at once, since the write backoff holds back the sweep's steps 1 and 3, not a client's upload | adopter | none | the sweep |
| list removed | in grace if the entry has a copy (a healing entry has one, as does a first write whose `put_new` succeeded), and the retries go on; otherwise leaves (R4) | | | |
| pin bump, policy change, restart | R5; R8; R3 (the unconfirmed copy is verified at start like any copy and may be the one that becomes `held`) | | | |

`unverified{error}`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| sweep, the read backoff passing included | the state table's `unverified` row (R6): `held` (an adoption from the CAS supersedes the unread copy; or step 2's re-read verifies); `fetching` under `fetch`, once the read count is lifted (the Nth failed read, or a re-read that records the copy damaged or `NotFound`); an adoption's state; or step 4's record (`unverified`, `corrupt` or `waiting`) | sweep | `unverified` after N | the next sweep |
| upload | adoption of a newer copy: `held`; the unread copy is superseded | adopter | none | not needed |
| adopt deferred or fail | R7; the read backoff and the read count are kept (R6) | adopter | per the next state | the sweep, by the next state's row |
| list removed | in grace: it has a copy (R4) | | | |
| pin bump, policy change, restart | R5, R8, R3 | | | |

`refused{host_not_allowed, address_filtered, not_https}` and `refused{over_cap}`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| upload (not `over_cap`) | adoption: `held`; otherwise R7, and the refused fetch gate is kept (R6) | adopter | clears when it holds | the sweep |
| sweep | the state table's row for the state (R6) | sweep | clears when it holds | the sweep |
| pin bump with other URLs (not `over_cap`) | R5: the refusal is reset and the sweep acts at once | sweep | clears when it holds | the sweep |
| a cap flag or the lists change (`over_cap`) | caps evaluated again ([section 7](#7-retention-and-caps)); if it fits, the sweep takes it | list API, start | clears | the sweep |
| `--mirror-host` changes (at a start) | R3, re-evaluated | start | clears | the sweep |
| policy change while the server runs | R8 | list API | per the next state | per the next state |
| no event | unchanged | | `refused`, naming the URL or flag fix | none: a person state |
| list removed, restart | R4, R3 | | | |

`denied`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| policy change from `deny`, at a start or by a change of its lists (R8) | the state the sweep records, at once | start, list API, sweep | per the next state | the sweep |
| list removed | leaves (R4) | list API | none | not needed |
| restart | R3 | | | |
| every other event | `denied`: nothing is fetched, adopted or indexed | | none (a decision, not a failure) | none: a person state |

`corrupt{copy}`

| Event | Next state | Who | Alert | Automatic recovery |
|---|---|---|---|---|
| sweep | the state table's `corrupt` row (R6): an adoption of the next copy from the CAS (`held`, `adopt_pending` or `store_write_failed`); `held` on an older copy that verifies; `fetching`, then as from `fetching`; or step 4's record | sweep, fetcher | `corrupt` until it holds, then per the next state | the sweep, which takes `store_write_failed` after its backoff |
| upload (`FindMissingBlobs` answers "missing" from the mark on, and the copy is `Absent` after a start) | fresh: stored in a segment and adopted | adopter | per the next state | the sweep |
| no event, policy `adopt`, no bytes on the farm | `corrupt` | | `corrupt`, naming one client build with a fresh daemon, or `--mirror-host <host>=fetch` | none: a person state, and the build uploads because the digest is reported missing |
| list removed | stays in grace and keeps healing (R4) | | | |
| pin bump, policy change, restart | R5; R8; R3 (re-index finds the damaged copy damaged again and tries the next lower, so `corrupt` or `held` on an older one) | | | |

**Paths back to `held`.**

| State | Automatic path | Waits for a client upload? | If no automatic path: the person's fix and its alert |
|---|---|---|---|
| `verifying` | the re-index, then the sweep | no | |
| `unverified` | the sweep's re-read; its adoption; or, once the read count is lifted, the fetcher | no | |
| `fetching` | the fetch | no | |
| `waiting` | the sweep, if the blob reaches the CAS; once the policy becomes `fetch` (R8), the fetcher | yes, and the digest is absent, so `FindMissingBlobs` does not suppress it | "not held after T": one client build, or `--mirror-host <host>=fetch` |
| `adopt_pending` | the next sweep | no | |
| `fetch_failed` | the fetcher's retry; the sweep's adoption of a present blob | no | |
| `upstream_drift` | the fetcher's retry; the sweep's adoption of a present blob | no | `upstream_drift`: re-pin (the entry is still retried) |
| `store_write_failed` | the retry from the spool; the sweep's adoption from the CAS; a copy of the store that verifies; if the CAS lost the blob, the fetcher under `fetch` | only under `adopt` with no bytes on the farm, where it becomes `corrupt` or `waiting` | after N, the store steps (the retries go on) |
| `refused` (not `over_cap`) | the sweep's adoption of a present blob | no | `refused`: another URL, or `--mirror-host` |
| `refused{over_cap}` | the sweep once caps are evaluated again | no | the cap flag, or retire lists |
| `denied` | none | | a policy change, if wanted |
| `corrupt` | the sweep: the CAS's bytes, an older copy that verifies, or the fetcher | only under `adopt` with no bytes on the farm, and then the digest is reported missing | `corrupt`: one client build, or `--mirror-host <host>=fetch` |

The defects earlier reviews found in this state machine, and how each was closed, are
in the history of this file; the prose and the tables above are the design.
