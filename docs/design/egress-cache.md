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
| Who reaches upstream | One fetcher module in `kbf-server`, off unless flags name the hosts it may reach. Build actions keep `--network=none`. |
| How entries are kept | Each held entry is one object under a stable key prefix, re-indexed at every start, kept while a live list names it plus a grace period. |
| How bytes get in | Fetched and verified by the fetcher, or *adopted*: a client upload of a listed digest is copied into the cache. No byte is accepted that does not hash to its key. |
| What buck2 needs | Nothing new. buck2 asks the CAS first; a held entry answers "present". |
| How a pin bump is covered | The bumping change's CI submits its list and waits until every new entry is held, failing with the entry, URL and error otherwise. |
| What a failure looks like | A state per entry in `GET /v1/mirror`, an alert naming the fix, and automatic recovery when the cause clears. |

## 2. Where things stand

### 2.1 Farm actions have no network

- The container driver runs every action with `--network=none` and `--pull=never`
  (`crates/kbf-driver-container/src/podman.rs`, `create_args`); the test
  `the_network_is_off` catches a missing flag. [daemon.md](daemon.md) lists networked
  actions as planned only.
- The native driver reads a `network` platform property (`network_of` in
  `crates/kbf-driver-native/src/network.rs`). It is enforced only where `sandbox-exec`
  is available; on Linux a network-on action has the node's network.
- `kbf-server` has no outbound HTTP client and kbf has no Remote Asset API: the vendored
  protos under `crates/kbf-proto/proto/third_party` are REAPI v2, ByteStream and their
  dependencies. `reqwest` is in the workspace, used by `kbf-objstore` (`S3Store`) and
  `kbf-mdm`.
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
| Upstream down or slow | every cold client stalls through its HTTP retries, then fails | held entries are served from the mirror store; nothing on the read path contacts upstream | report `fetch_failed{host, error, since}` on entries not yet held; alert after N failed attempts, naming the host and the entries at risk; retry with backoff, clearing on success |
| Upstream serves different bytes at the same URL (a regenerated archive, a moved tag, a compromised host) | builds fail everywhere until someone re-pins | bytes are accepted only if both SHA-256 and size match; a held copy is never replaced; new bytes are never accepted automatically | report `upstream_drift{url, observed_sha256, observed_size}`; alert "the farm still serves the held copy; re-pin to a stable asset or vendor it"; clears when no live list names the old digest |
| A wrong pin in a change | that change's build fails | the same check: nothing is stored; the CI wait step fails the change naming the entry | reported on the entry; no alert, because the change's own check is the signal |
| A pin bump hits many runners at once | N clients fetch upstream together | the list is submitted before the build; one fetch per digest; a per-host concurrency cap; the first verified copy, fetched or adopted, serves everyone | in-flight count and queue depth per host; no alert unless a fetch fails |
| Cache poisoning | not applicable | the digest is the only key; a URL is a hint; every byte is verified before commit and on every read; nothing is keyed by URL; the action cache is never written | counter `mirror_rejected_total{reason}` |
| The fetcher used to reach internal addresses | not applicable | HTTPS only; a host allow-list by flag; DNS resolved once per hop and the connection made to that address; loopback, private, link-local and unique-local addresses refused at every hop; the fetcher fetches only listed entries, never a URL given at miss time | refusal recorded on the entry; alert naming the flag to change |
| A source's licence forbids a private copy | not applicable | a per-host policy: `fetch`, `adopt` or `deny` ([section 5.5](#55-per-host-policy)) | the report shows each entry's policy; a `deny` entry is reported, not alerted |
| Store pressure | not applicable | per-list and total byte caps; over a cap, new entries are refused and held entries are never evicted | alert at 80% of the total cap and on any refusal, naming the cap; recover by raising it or retiring lists |
| Server restart | cold CAS | held entries live under a stable prefix and are re-indexed at start | re-index count and duration in the report; alert if the re-index fails |
| A damaged object in the store | not applicable | every read re-hashes; a bad object becomes `Unavailable`, which `FindMissingBlobs` reports as missing, so the client uploads again and the upload heals it | report `corrupt`; alert; refetch under `fetch`, or wait for an adopting upload |

## 5. The mechanism

### 5.1 Fetch lists

- A list is JSON lines, one entry per line: `{sha256, size, urls[], label}`. `label` is
  free text for reports (for example the rule and asset name). A list has a name and a
  generation number that only increases.
- `PUT /v1/mirror/lists/{name}` replaces a list with a newer generation; an older or
  equal generation is refused. `DELETE` retires it. `GET /v1/mirror` is the report of
  [section 8](#8-report-alert-recover).
- Each entry must state a 64-hex-digit SHA-256 and a positive size. An entry without
  either, a size over the per-entry cap, or a URL that is not `https` is refused at
  submission with a reason, and the whole submission is refused, so a list is never
  half applied.
- Caps by flag: entries per list, bytes per list, and total bytes across lists. A list
  over its caps is refused with the totals in the error.
- A list carries an optional expiry. CI sets one on lists from unmerged changes (the
  lean is 14 days); lists from the default branch and release branches have none and
  live until replaced or retired.
- The writes go through the operator API and its gates ([api.md](../api.md#who-may-write)).
  Who may submit is an open decision ([D2](#d2-how-lists-arrive-and-who-may-submit)).
- kbf knows nothing about any client repository. A list is data.

### 5.2 The mirror store and its location

- A held entry is one whole object at `<mirror-prefix>sha256/<hex>/<size>`, written
  with `put_new` and never overwritten. `--mirror-prefix` is a flag and does not change
  between starts. Where the store claims `conditional_put`, a second write of the same
  key is refused by the store; where it claims `object_lock`, the object is written with
  a retention date at least the grace period of [section 7](#7-retention-and-caps).
- The object is not packed into a segment, so it can be listed, inspected and removed on
  its own. Entries are few (hundreds), so this costs little.
- Today's `Location::Object(ObjectId)` (`crates/kbf-meta/src/model.rs`) names an object
  by a counter under the per-start prefix, so it cannot name a key that survives a
  restart. The cache adds one variant, `Location::Mirror`, whose key is computed from
  the digest. Reachability is tracked for it the way `ObjectUnreachable` and
  `ObjectReachable` track segment objects. When the planned "several stores" location
  model lands ([storage.md](storage.md#planned)), `Location::Mirror` becomes a location
  in a named store.

### 5.3 Re-index at start

- At start, before the REAPI listener accepts calls, `kbf-server` lists
  `<mirror-prefix>sha256/` (`ObjectStore::list`, paged) and commits a `PutBlob` with
  `Location::Mirror` for each key that parses as a digest. Listing hundreds of keys is
  fast; no object's bytes are read.
- The bytes are checked on first read, as every read is (`Cache::read_blob` re-hashes
  what `get_range` returns). An object whose bytes do not match its key is marked
  unreachable, so the blob reads `UNAVAILABLE` and `FindMissingBlobs` reports it
  missing: the client uploads it, the upload heals the entry, and the bad object is
  reported as `corrupt`. A damaged object never turns into a failed build, and is never
  reported absent.
- A key that does not parse is reported and left alone; nothing is deleted at start.
- A failed listing fails the re-index: the server reports it and raises an alert, and
  starts without the mirror rather than serving a partial view as complete.

### 5.4 Adopt on upload

- When `Cache::store_blobs` commits a digest that a live list names and the mirror
  store does not hold, the cache queues a copy of the verified bytes to its mirror key.
  The upload is acknowledged as today, after the segment commit; the copy is a
  background write on a bounded queue, and a lost copy is redone by the next upload or
  by the fetcher.
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
- **Connections.** HTTPS only, with rustls and the system roots. For every hop the
  fetcher resolves the host once, refuses the answer if any address is loopback,
  private, link-local (which covers cloud metadata services), unique-local or
  unspecified, and connects to the address it checked, so a second lookup cannot return
  something else. Redirects are followed up to `--mirror-max-redirects` (lean 5), each
  hop checked again against the host list and the address filter. The fetcher sends no
  credentials and no cookies.
- **Streaming.** The body is written to a spool file under `--mirror-spool-dir` on local
  disk, not held in memory, while the fetcher counts bytes and computes SHA-256. It
  aborts at `size + 1` bytes. `Content-Length` is never trusted (GitHub source archives
  send none).
- **Commit.** Only when the length equals the size and the hash equals the SHA-256 is
  the spool written to the mirror key with `put_new`, read back by range and re-hashed,
  and then committed as `PutBlob` with `Location::Mirror`. Any mismatch stores nothing
  and records `upstream_drift` with what was observed.
- **Candidates.** `urls[]` are tried in order; each is verified the same way.
- **Concurrency.** One fetch per digest at a time (later callers wait on it), at most
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
   meets it first.
4. The build runs after that step and gets CAS hits.
5. On merge, the default branch's list is submitted with the merged pins. The change's
   list expires.

A build that starts before step 3 finishes fetches upstream itself, as today, and its
upload is adopted. A change from a fork cannot hold the submission credential, so its
new pin is fetched by the client as today and is not adopted (it is on no list) until
the merge submits it.

### 6.2 A CAS miss after eviction or a restart

Held entries are not evicted, and the re-index of [section 5.3](#53-re-index-at-start)
brings them back after a restart. A listed digest that is still absent (its fetch
failed, or its host is `adopt` and no client has uploaded it) is reported missing as
today and the fetcher is woken.

### 6.3 Upstream outage

Held entries are served with no upstream contact. Entries not yet held fail on the
client as they do today, and the report and alert name them and their host. Recovery
needs no person: the fetcher's retry succeeds when the host returns, or any client that
can reach the file uploads it and it is adopted.

### 6.4 Upstream drift

A held entry is never refetched, so drift is seen only when a fetch runs: a new list, a
retry, or the later scrub. An entry that is only adopted is never compared with
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
  removed by the planned collector, never silently.
- Caps: per entry, per list and in total, all flags. Over a cap new entries are refused
  with a reason; a held entry is never evicted to make room.
- The total is reported, with the share each list holds.

## 8. Report, alert, recover

**Report.** `GET /v1/mirror` lists, per entry: digest, labels, the lists and
generations that name it, its host policy, its state and the last error. States:

| State | Meaning | Clears when |
|---|---|---|
| `held` | in the mirror store and indexed | |
| `fetching` | a fetch is running | the fetch ends |
| `waiting` | listed, host `adopt`, no upload yet | a client uploads it |
| `fetch_failed{host, error, attempts, next_retry}` | the last fetch failed | a retry succeeds |
| `upstream_drift{url, observed_sha256, observed_size}` | upstream served other bytes | no live list names the digest |
| `refused{host_not_allowed, address_filtered, over_cap, not_https}` | the farm will not fetch it | the flag or the list changes |
| `denied` | host policy `deny` | the policy changes |
| `corrupt` | the stored object failed its digest on read | a fetch or an adopted upload replaces it |

Totals: held bytes, caps, entries per state, re-index count and duration of this start.

**Metrics.** `mirror_fetch_total{host, result}`, `mirror_bytes_fetched_total`,
`mirror_bytes_held`, `mirror_adopted_total`, `mirror_client_fetched_total`,
`mirror_rejected_total{reason}`, `mirror_hits_total` (present answers for listed
digests).

**Alerts.** Each names the entry, the host and the exact fix:

| Alert | Fix it names |
|---|---|
| an entry of a default-branch list not held after T | the URL and error; the fetcher keeps retrying |
| `upstream_drift` | do not accept the new bytes; re-pin to a stable asset or vendor the file |
| a host failing for longer than T | the entries at risk |
| a refusal | the flag to change (`--mirror-host`, a cap) |
| total held bytes at 80% of the cap, or any refusal over a cap | the cap flag, or the lists to retire |
| re-index failed, or a mirror-store write or read failed | the store error |
| `corrupt` | none needed under `fetch` (it refetches); under `adopt`, a rebuild that uploads it |

`kbf-alert` has no code yet (`crates/kbf-alert/src/lib.rs` is a module comment). v0
exposes the alert states in the report, and alert delivery is its own issue.

**Recover.** Every state in the table clears by itself once its cause is fixed.

## 9. The client's side (komira)

- **No rule changes.** `pinned_file`, `crates_io_library` and `oci_base` already state
  (SHA-256, size), and buck2 already asks the CAS first.
- **One generic export target** writes the fetch list (SHA-256, size, URL, label) for
  every declared download in the build graph: the platform table, the crate rule, the
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
- Address filter: loopback, private, link-local, unique-local, an `http://` URL, a
  redirect into a private range, a DNS answer that changes between two lookups. Mutants:
  check only the first hop; resolve again at connect.
- List validation: no SHA-256, size 0, over each cap, an older generation. Mutant: apply
  the valid half of a bad submission.
- Policy: an `adopt` host is never fetched; a `deny` host is never adopted. Mutant:
  `adopt` fetches.

**Integration** (`kbf-it`, an in-process fake upstream with fault injection, on
`MemoryStore` and on the object-store conformance targets):
1. A first fetch fills the entry, and the fake upstream sees exactly one request for N
   concurrent wakes. Mutant: remove single-flight.
2. Fetch, restart (a new per-start prefix), then `FindMissingBlobs` reports present and
   the read re-hashes, with zero upstream requests. Mutants: write under the per-start
   prefix; skip the re-index.
3. Adopt on upload, restart, hit, with the fetcher off. Mutant: adopt only when the
   fetcher is on.
4. Upstream serves other bytes: `upstream_drift`, nothing stored, the held copy
   unchanged, the action cache untouched.
5. A mirror object damaged in the store: the read fails `UNAVAILABLE`,
   `FindMissingBlobs` reports missing, an upload heals it. Mutant: report the damaged
   entry present.
6. A digest on no list: zero upstream requests and nothing adopted.
7. The cap refuses a new entry and never evicts a held one. Mutant: evict the oldest.
8. Upstream down, then up: `fetch_failed`, alert state set, then `held` with the state
   cleared.

**Simulation** (a new family in `kbf-sim`, on the virtual clock, in the conventions of
[simulation.md](simulation.md)): upstream up, down and drifting at random; list
generations and client uploads interleaved; restarts. After every step: a committed
mirror object hashes to its key; at most one fetch per digest is in flight; no entry is
lost while listed or in grace; every listed entry that is not `held` carries a reason;
nothing outside the lists is fetched. At the end: every state whose cause healed has
cleared. Mutants: drop single-flight; drop the re-index; let a failed fetch clear the
entry.

**End to end**, on a deployed farm, in the shape of komira#563's proof:
1. A cold farm and a client whose upstream for one pin is unreachable: the build fails.
2. Submit the list with the true URL; the fetcher holds the entry; the same build passes
   with every action remote and no upstream contact.
3. Restart the server; the build passes again with no upstream contact.

## 12. Order of work

**v0:** the list API with validation and caps; `Location::Mirror`, the mirror store and
the re-index; adopt on upload; the fetcher with the host policy, address filter,
single-flight and backoff; the report and metrics; the client's export and CI step.

**Later:** alert delivery through `kbf-alert`; the OCI token exchange; a scrub that
re-reads held entries; holding `FindMissingBlobs` during a fetch; an HTTP blob endpoint
by digest; a Remote Asset front; moving the mirror index onto the replicated metadata.

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
| `kbf-server` has no outbound HTTP client; no Remote Asset protos are vendored | V | `crates/kbf-server`, `crates/kbf-proto/proto/third_party` |
| A restart empties the CAS; nothing expires while the server runs | V | `crates/kbf-server/src/config.rs`, [storage.md](storage.md) |
| `Location::Object` names an object under the per-start prefix | V | `crates/kbf-meta/src/model.rs`, `Cache::object_key` |
| `Unavailable` is reported missing by `FindMissingBlobs`, so an upload heals it | V | [storage.md](storage.md#three-answers-not-two) |
| buck2 has no Remote Asset client | V on buck2 `main`, by search | facebook/buck2 |
| `download_file` probes the CAS and needs 2 hours of remaining life | V on buck2 `main` | `download_file.rs` |
| The open-source client gives a present blob `cas_ttl_secs` (3 hours default) and caches answers for up to 12 hours | V on buck2 `main` | `remote_execution/oss/re_grpc/src/client.rs` |
| The release komira pins behaves the same at the CAS boundary | A; measured for hits by komira#563, not read in its source | komira-ai/komira#563 |
| Writing a held file locally reads it from the CAS after the probe | A, read from the source, not tested | `download_file.rs` |
| A public registry blob GET may need an anonymous token | A | to be checked per registry |
| The working set is about 1 GiB | A, a rough grep | section 2.5 |
| Each source's terms allow a private copy | A, not checked | D3 |
