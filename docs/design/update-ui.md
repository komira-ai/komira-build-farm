# The update page: versions per host, update available, update action

Status: design. Nothing in this file is built. Facts marked **[F]** were read from the code on two lines of work, named
here because the page needs both: **arch-v2** (`760fbb06`: `kbf-ui`, `GET /v1/members`, `--api-listen`, `ui-data.md`,
`split-mode-updates.md`) and **pull-updates** (`6561a838`, not yet containing arch-v2: `kbf.update.v1`, the release store
in `release/http.rs`, the `Read by a later change` arm at `worker.rs:295`, and the frozen specification
`pull-updates.md` 6.3, which this file calls "the spec"). Wherever the text says "the update branch" it means pull-updates;
"this branch" is not used. **[D]** is decided in another document; **[P]** is proposed here; **[Q]** is an open question
(section 8).

## 1. Summary

An operator wants to see, on one page, which commit each host runs, which release the farm wants, whether each host's update
step is done or stuck, and to press "Update to <release>". Today almost none of that is readable.

- **[F]** The frontend receives each host's `UpdateStatus` (`kbf.update.v1`) and discards it (`worker.rs`, the `Offer(_) |
  UpdateStatus(_)` arm returns nothing, "Read by a later change").
- **[F]** There is no desired-release state, no update policy and no update route. The one update route built is the release
  store (`PUT` and `GET /v1/releases`).
- **[F]** The UI is read-only and has no form, POST, session or CSRF handling (`kbf-ui`, `ui-data.md` section 8). Every write is
  gated on a loopback peer, no `Origin` header and a bearer token, so a browser form cannot reach it by design (`api.rs`).
- **[F]** No `kbf update` CLI verb exists on either line of work. Today the only way to call a write route is `curl`.

So the work is a read path first (routes, then a read-only page), and a write path second, which needs one decision (Q1) before
any code. This document designs both and says what blocks each. Rollout order and the per-host step belong to
`split-mode-updates.md`; the wire and the release store to `pull-updates.md`; this file only decides what the page reads, how
it writes, and how it is tested.

## 2. Rules the page obeys (from `ui-data.md`)

1. **Pages read the JSON API only** (6.1). Each page is a `Page { name, reads }`; its `Reader` refuses a URL that matches no
   declared pattern, and the page lists every URL it read as a link. The Updates page declares `/v1/update`,
   `/v1/update/hosts`, `/v1/update/plan`, `/v1/nodes` and `/v1/releases`, and `/v1/members` if the role column needs it
   (Q4; the role column depends on `/v1/members`, which is on arch-v2 and pull-updates both, not on the update routes). It
   parses into `kbf-ui`'s own `api.rs` shapes, never the server's structs. **Every declared read is a GET**: the
   Reader/`PAGES` contract cannot declare a POST, and a page never causes a write-scoped call, directly or through
   the server it runs in.
2. **Escaping by construction.** All markup goes through `html::Html`, which escapes every string and has no raw write. A
   commit, release name, refusal text, hold reason and node id are strings from hosts and are rendered as text. Red test: a
   refusal of `<script>alert(1)</script>` renders escaped.
3. **CSP and no script.** Every `Answer` carries `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self';
   script-src 'self'; frame-ancestors 'none'` and `X-Content-Type-Options: nosniff`. The page has no JavaScript: the confirm
   step is a second page, not a dialog.
4. **Page route contract.** The page is added to the `PAGES` array, so the contract test checks that every URL it reads is a
   declared route, and rejects dot segments, backslashes and control characters.
5. **Built fields keep their names; new keys are additive.** Timestamps are RFC 3339 strings, durations and sizes carry units
   (`_ms`, `_bytes`), and absent means not measured, never zero. A host that never sent `UpdateStatus` is `unsupported`, not
   `idle` (`pull-updates.md` section 5); it is `availability: unsupported`, a key the spec defines (section 4), not an
   absent key.
6. **Instance scoping and authority.** Update data names no build and no instance, so it is a fleet read, and the
   per-instance `invocations.getAuthorizer` filter of `ui-data.md` does not apply to it. It is still not public: the
   `refusal` text, staged and installed commits, `helper_present`, the desired release, the policy and the plan are an
   attack map of the update path on a private farm. They go under `farm.get` (Q3), not open. `ui-data.md` 8 keeps `/v1/nodes`
   open only because it names no build and scrapers use it; the update fields on it are limited to the same standard
   (section 4). The confirm page must not list builds or invocations from a plan's "failed builds during the run" (a column of
   split mode) without the `invocations.get` check, because that column names instances.
7. **Add the routes to the route table** in `ui-data.md` 6.2 in the same PR as each route.

## 3. What the page shows

Per host (a table, one row per node, sorted by node id; the page size and token are the API's `page_size` and `page_token`):

| Column | Source | Today |
|---|---|---|
| node, connected | `/v1/nodes` `node_id`, `connected` | **[F]** built |
| platform | `/v1/nodes` `software.os_name`, `os_version`; capability `arch` | **[F]** built, arch by inference from Hello |
| role (storage-and-daemon, daemon-only, Mac) | none | gap: `NodeView` has no role; `/v1/members` is keyed by member id (Q4) |
| running commit | `/v1/update/hosts` (`UpdateStatus.running_commit`) | wire exists, dropped by the server |
| desired commit | `/v1/update` desired | no state, no route |
| update step state | `/v1/nodes` `update.state`, from `UpdateStatus.state` (IDLE, FETCHING, STAGED, ACTIVATED_UNCONFIRMED, CONFIRMED, REVERTED, REFUSED, MANUAL) | wire exists, dropped |
| last error | `/v1/update/hosts` `refusal` (`UpdateStatus.refusal`) with state REFUSED or REVERTED | wire exists, dropped; no machine-readable reason (Q6) |

The controller's richer phase (selected, cordoned with N leases left, waiting for write sessions, restarting, rebuilding,
held with a reason) is the `update/host/<id>` row. **[F]** No controller exists (`split-mode-updates.md` U4), so the first page
shows only the wire state and says so in a note; it does not invent phases. **Step state without and with the controller.**
The column is always the host's own report (`UpdateStatus.state`); the controller's phase, when it exists, is a second
column beside it, never a replacement, so the first column does not change meaning. A host with no controller row shows
its wire state only; a controller row for a host that is silent shows the phase with the wire state as `availability:
unavailable`. A host's wire state can disagree with the controller's phase; the page shows both and does not reconcile.

Farm (a header block above the table):

- available releases: `/v1/releases`, each commit with its platforms and whether each is complete;
- desired commit and generation: `update/desired` `{generation, commit, set_at}`, shown as "none" before any is set;
- policy: `update/policy` (`paused`, `auto_nodes`, `auto_frontend`, `max_hosts_out`, and the rest), read-only in the first page;
  the spec's `POST /v1/update/auto` and `POST /v1/update/policy` exist and stay out of scope (Q5);
- counts ("N of M hosts up to date"): the **page** sums the per-host `up_to_date` values it already reads; the server
  does not also return counts, so there is one derivation and a script and the page agree on `up_to_date`.

Per-host "up to date" is derived by the server and delivered as `update.up_to_date` (true, false or null), not computed in
the page (the spec, section 6.3).

## 4. Routes

Built (**[F]**): `GET /v1/nodes`, `GET /v1/releases` (503 without `--releases-dir`; under the spec `GET /v1/releases` and
`Fetch` carry no authentication of their own, `release/mod.rs`, so the release list is open, which this design does not
change), `PUT /v1/releases/...`, the node cordon, drain and uncordon posts, `GET`/`POST /v1/frontend/drain`, `GET
/v1/members` and `POST /v1/members/{id}` (arch-v2 and pull-updates both).

**Alignment with the spec.** The spec (`pull-updates.md` 6.3, frozen) says: `GET /v1/update` is `{desired, auto_update,
releases}`; `GET /v1/nodes` gains a per-node `update` object with `state`, the three commits, `offered_generation`,
`refusal`, `helper_present`, `up_to_date` (true, false or null) and `availability` (`ok`, `unavailable`, `unsupported`);
`POST /v1/update/desired {commit}` is a compare-and-set on `update/desired`; `POST /v1/update/auto` and `POST
/v1/update/policy` exist. This design uses those shapes unchanged and **adds**, each an amendment of the spec text made in
the PR that builds it (not silently):

- `GET /v1/update` keeps `{desired, auto_update, releases}` and gains `policy` (the `update/policy` row). No counts. (PR 2)
- `availability` is how the page tells a node that never sent a status (`unsupported` or `unavailable` with no `state`), one
  that sent it and went quiet (`unavailable` with the last `state`) and one running an old binary (`unsupported`).
  Absence of the key is never read as `unsupported`. (PR 1)
- `GET /v1/update/hosts` (**new**, paginated by `page_size` and `page_token`): the full per-node `update` object for every
  node. The spec puts the full object on `GET /v1/nodes`, which is open (`api.rs`: reads answer anyone who reaches the
  listener). To keep the free-text and commit fields off an open route, **`GET /v1/nodes` carries only `update: {state,
  up_to_date, availability}`** and the rest (`running_commit`, `installed_commit`, `staged_commit`, `offered_generation`,
  `refusal`, `helper_present`) is on `/v1/update/hosts`, under `farm.get`. This narrows the spec's `/v1/nodes` object and
  is amended in pull-updates 6.3 in PR 1. `refusal` is host-supplied free text: the server caps it (512 bytes, cut on a
  character boundary, control characters replaced) on the way in, in addition to the page escaping it. Held in memory per
  node, newest wins, a status from a replaced stream is ignored as `NodeStatus` is (`pull-updates.md` 6.4), refilled at the
  next `UpdateStatus` after a restart, like `software` (the `fleet.rs` header).
- `POST /v1/update/desired {commit, expect_generation, plan_digest, plan_issued_at}`: the spec's compare-and-set plus three
  request fields, **new**. 409 if no platform is complete for the commit, 409 `stale_generation` if `expect_generation` is
  not the current generation, 409 `stale_plan` under the rules below, no new generation if the commit equals the current
  one. Operator gate, as the other writes. A request without `plan_digest` is allowed from a script (the spec's shape)
  only when `--allow-unplanned-update` is on, default off, so the confirm is not skippable by accident.
- `GET /v1/update/plan?commit=<c>` (**new**, a read): the preview. **It is a GET on the read authority, not a POST.** It
  takes a commit in the query, writes nothing, and is computed from the current state each time (no stored plan). It
  returns the hosts that would change (each with its from-commit and to-commit), the order a rollout would use, the
  blockers that would stop it, `plan_issued_at` and `plan_digest`. Until the controller exists (U4) it lists hosts and commit
  changes only and says "order: not planned". The answer is `Cache-Control: no-store`.

**`plan_digest`, exactly.** SHA-256 over the bytes `"kbf.update.plan.v1\0"` followed by the canonical JSON (UTF-8, object
keys sorted bytewise, no whitespace, integers as decimal, no floats) of

```text
{ "commit": <40 hex>, "generation": <current update/desired generation, or 0>, "issued_at": <RFC 3339, seconds, UTC>,
  "hosts": [ {"node_id": s, "from": <installed commit or null>, "to": <commit>} ... sorted by node_id ],
  "blockers": [ {"code": s, "node_id": s or null} ... sorted by (code, node_id) ] }
```

Included: the target commit, the generation the plan was computed against, the issue time, the host list with
installed and target commits, and the **stable blocker set** (codes: `no_complete_platform` per platform,
`host_unsupported`, `helper_missing`, `paused`). Excluded, because they change on a reconnect or a status tick and
would make the confirm page unusable on a live farm: `connected`, step state, `refusal`, `offered_generation`, queue and
lease counts, timestamps other than `issued_at`. A host that is offline is a row, not a blocker.

**Check at apply time.** `POST /v1/update/desired` recomputes the plan from the current state with the request's
`plan_issued_at`, and refuses `stale_plan` unless (1) the recomputed digest equals `plan_digest` (so a changed host list,
blocker set or generation refuses), (2) `expect_generation` equals the current generation, and (3) the plan is not older
than 10 minutes and not more than 5 seconds in the future. The client cannot extend the expiry: `plan_issued_at` is in the
digest, and the server checks it against its own clock.

All new routes are documented in `docs/api.md` and listed in `GET /v1` and `/llms.txt`.

## 5. The action, authorization and confirmation

**Confirmation is built into the page [P].** "Update to <release>" is a link to `GET /ui/updates/confirm?commit=<c>`, which
reads `GET /v1/update/plan?commit=<c>` through the page's declared reads (a plain GET, the same authority as every other
read the page makes), shows the plan (hosts, from-to commits, blockers) and the exact command with the commit, the
generation, the digest and the issue time filled in. A blocker renders the command as text with the reason and marks it not
to be run. **No GET on the UI causes a write-scoped call, and the UI process holds no operator token.** No single click
changes the farm.

**Authorization today.** **[F]** A write needs a loopback peer, no `Origin`, the bearer token and a JSON content type. A
browser cannot send that, and `ui-data.md` 8 states the UI is read-only and a cookie must never make a POST. Two ways forward:

- **(a) Read-only page plus a command [P, the plan].** The confirm page shows a command to run where the token is. **The
  command that works today is `curl`**: `curl --header "Authorization: Bearer $(cat <token-file>)" --header 'Content-Type:
  application/json' --data '{"commit": ..., "expect_generation": ..., "plan_digest": ..., "plan_issued_at": ...}'
  http://127.0.0.1:<api-port>/v1/update/desired`, with the token read from the file named by `--api-token-file`, never
  placed on the command line or in the page. A `kbf update set --commit <c> --plan <digest>` verb **does not exist** on
  either line of work; it is **new work (PR 6, section 7)** and until it lands the page shows the `curl` form only. Nothing
  in the security model changes.
- **(b) A UI write path [Q1].** Designed in `split-mode-updates.md` 6.3: a session made from the operator's credential, a CSRF
  token in every form, `SameSite=Strict`, a server `Origin` check, roles (read, update, admin) and an audit record naming the
  actor. **This reverses a decision, not an open point**: `ui-data.md` 8 (Q16) decided a reverse proxy for v1 and "no cookie
  path until a POST is proven cookie-proof", so (b) needs that decision reopened, by a separate design document before any
  code (it is not part of this one's PR list). That document must answer three things this file does not: (1) the operator
  write gate requires a loopback peer and no `Origin`, and a session POST arrives from a remote browser, so either the gate
  is loosened (a change to `api.rs`'s documented invariant) or the UI mount calls the API on loopback with a stored
  operator token, which is a confused deputy and is refused here; (2) the roles read, update and admin exist only as plans (the
  code has `Daemon` and `Client`); (3) the audit record needs a home: a state key, its retention and who reads it. It also
  needs the TLS read listener (Q18 there). **(a) is the plan.**

Whichever is chosen, the write is the same `POST /v1/update/desired`; the page never writes the state store itself, and an
update to the update-controller is never a UI-only code path.

## 6. What exists and what to build

| Piece | State |
|---|---|
| `kbf-ui` page framework: `Page`, `Reader`, `Html`, CSP, contract test | built, GET only |
| `GET /v1/nodes`, `/v1/releases`, `/v1/members` | built (`/v1/releases` mounting is the open `--releases-dir` change) |
| `UpdateStatus` on the wire | built, ignored by the server |
| `update` object in `NodeView` (open summary) and `GET /v1/update/hosts` (detail) | to build (PR 1) |
| `update/desired`, `update/generation`, `update/policy` rows, `GET /v1/update` | to build (PR 2) |
| `POST /v1/update/desired` (with `expect_generation`, `plan_digest`) and `GET /v1/update/plan` | to build (PR 3) |
| Updates page, read-only, plus the confirm page with the `curl` command | to build (PR 4) |
| `farm.get` authority on the update reads | not built (`ui-data.md` Q11); blocks PRs 1 (detail), 2, 3 from serving to anyone but a token holder |
| `kbf update` CLI verb (`set`, `plan`) | new work (PR 6) |
| UI session, CSRF, `Origin` check, roles, audit, form POST | not part of this design; a separate document first, only if Q1 reopens (b) |
| Controller phases, blockers, held reasons | not started (`split-mode-updates.md` U4, U10) |

## 7. PR split and tests

Each PR is red first: the test is committed failing against the old code, then the change makes it pass. Each lists a mutant
that must turn it red.

1. **Server keeps `UpdateStatus`; `NodeView.update`; `GET /v1/update/hosts`.** Tests: a host sending a staged-and-refused status
   shows it on `/v1/update/hosts` and only `{state, up_to_date, availability}` on `/v1/nodes`; a host that never sent one has
   `availability: unsupported` or `unavailable` (the spec's rule) and the page later says so; a second status replaces the
   first; a restart empties it; **a status from a replaced (stale) stream is dropped**; a 100 KiB `refusal` is stored cut
   to 512 bytes on a character boundary. Mutants: keep the oldest instead of the newest; default to `idle` for a silent host;
   accept a status from the old stream; store `refusal` uncut; put `refusal` on `/v1/nodes`. Amends `pull-updates.md` 6.3.
2. **Desired-state rows and `GET /v1/update`.** Tests: empty farm answers `desired: null` and the spec's `auto_update`;
   after a set, the row agrees with `/v1/nodes`. Mutant: drop `auto_update`.
3. **`POST /v1/update/desired` and `GET /v1/update/plan`.** Tests: 409 for an incomplete platform; same commit gives the same
   generation; different commit increments it by exactly one; stale `expect_generation` is 409 `stale_generation`; a stale
   digest, an expired plan (older than 10 minutes), a plan from the future and a plan whose `issued_at` was edited are 409
   `stale_plan`; **a host reconnecting or changing step state between the plan and the apply does not change the digest**, a
   changed host list or blocker does; the plan GET writes nothing (a counting store) and works without a token on the read
   authority and is refused for a POST; no token, an `Origin` header or a non-loopback peer is refused on the POST as for other
   writes. Mutants: take the generation on every call; ignore the digest; **digest covers a volatile field (`connected`)**;
   skip the expiry; skip the generation in the digest. Amends `pull-updates.md` 6.3.
4. **The page.** Tests: every `availability` and every state of `UpdateStatus.State` renders; a refusal string and a node id
   with markup render escaped; CSP and `nosniff` are on the answer; the page is in `PAGES` and the contract test fails if it
   reads an undeclared URL **or declares a non-GET**; a blocker marks the command not to be run and shows its reason; the
   confirm page issues only GETs (a counting `Source` sees zero writes and the UI process holds no token). Mutants: raw-write a
   refusal; read `/v1/update` from an undeclared pattern; route the plan through a POST; omit the blocker reason.
5. **(unused; was the UI write path, now a separate design, section 5 (b).)**
6. **`kbf update` CLI** (`set --commit --plan`, `plan --commit`), on the existing write gates, reading the token from
   `--api-token-file`. Tests: the verb sends the four request fields; a refused plan prints the 409 reason; no token on the
   command line. It is not required for the page.

PRs 1 to 4 are independent of the controller. PR 4 is mountable only when the crate is mounted on a server, which is not yet
the case (`kbf-ui` lib docs: "Not mounted on a server yet").

## 8. Open questions

- **Q1 UI writes.** Choose (a) `curl` (and later the CLI) only, or (b) a session and CSRF write path. Recommendation: (a). (b)
  reverses `ui-data.md` Q16 and needs a separate design first, which answers the gate, roles and audit questions of section 5.
- **Q2 Which credential makes a UI session.** Not settled in `split-mode-updates.md` 6.3. Needed only for (b), and so only
  for its own document.
- **Q3 Authority of the update reads.** Recommendation: `farm.get` for `/v1/update`, `/v1/update/hosts` and
  `/v1/update/plan`, and for every field beyond `{state, up_to_date, availability}`; `/v1/nodes` stays open with that
  summary. `farm.get` is not built (`ui-data.md` Q11), so until it is, those routes are served only to a bearer-token
  holder (a read gate that does not require loopback), and the page reaches them through the reverse proxy of `ui-data.md`
  Q16. **Not** "open because commits are public": on a private repository the commits, host `refusal` text and policy are
  not public.
- **Q4 Role.** Add `role` to `NodeView`, or have the page join `/v1/members` by node id. Recommendation: `NodeView.role`, so
  the page makes one read per concern and the join is not duplicated in every consumer. Needs a node-to-member mapping that the
  code does not give today.
- **Q5 Skip, Roll back, Accept failing builds, Policy.** Out of scope here; each needs its own confirm page and the admin role,
  and they depend on the controller.
- **Q6 Machine-readable refusal.** `refusal` is free text. Until a reason code exists the page shows the text only and groups
  nothing by it.
- **Q7 Branch state.** The facts come from two lines of work that have not been merged (section header). PR 1 needs the
  `UpdateStatus` wire and `NodeView` together, so it stacks on pull-updates after arch-v2 is merged into it; if that has not
  happened, `GET /v1/members` and the `kbf-ui` crate are missing there.
- **Q8 `/v1/releases` is open under the spec.** It lists commits and platforms to anyone who reaches the listener. Whether it
  moves under `farm.get` is a change to the spec; this design leaves it open and flags it.

## 9. What blocks implementation

- The change that mounts `--releases-dir` and the release fetch service on the servers (open) blocks `/v1/releases` being
  served, so blocks the releases list and the plan's completeness check.
- The server reading `UpdateStatus` (PR 1 above) blocks every per-host column.
- The root helper (open) blocks hosts reporting `helper_present` and real step states in production, not the page itself.
- The controller (U4) and poller (U10) block per-host phases, held reasons and preflight blockers, and are not needed for the
  first page.
- `farm.get` (not built) blocks serving the update reads to anyone but a token holder (Q3).
- The TLS read listener and a reopened Q16 block (b), which is not in this design's PR list.
- PR 6 (the CLI verb) blocks nothing but the page showing a CLI command instead of `curl`.
