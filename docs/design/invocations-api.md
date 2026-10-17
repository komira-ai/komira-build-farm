# Invocations: the API an agent reads

This is the agent-facing part of [invocations.md](invocations.md): what an agent needs,
the failures view (4.7), the client (4.8), read tokens (4.9) and a record that is gone
(4.10). It keeps that document's section numbers, so a bare number here (3.5, 4.3,
section 9, PR 12) is a section of [invocations.md](invocations.md); the id, the record,
[where it is kept](invocations.md#35-where-it-is-kept), the
[routes](invocations.md#41-routes) and 4.2 to 4.6, the link, the failure modes and the
[implementation plan](invocations.md#9-implementation-plan) are there.

**Everything in this document is planned** unless it says "today".

## What an agent needs

An agent has the id (buck2's `Build ID`, `--write-build-id`, or the link of 5.2) and a
question. It runs one command, `kbf build show <id>` (4.8), which prints one JSON
answer: the invocation's result and counts, every failed, timed-out and refused action
with its reason, attempts, command and log tails, and absolute links to everything
else. Every link in that answer is read with the same command.

| Flow | What the agent gets | Calls |
|---|---|---|
| (a) Did it pass; which actions failed and why | the invocation's `state`, `complete` and counts, and every failed, timed-out or refused action: its outcome, exit code, failure class, the farm's reason text for an INFRA failure, and its attempts (each rerun and why it was rerun) | one: `kbf build show <id>`, which reads `GET /v1/invocations/{id}/failures` (4.7); past one page of failures, the answer's `next_page` link |
| (b) A failed action's stderr and stdout | a bounded tail of each, inline in the answer of (a), so the common case needs no second call; the whole log, uncut, by its link | none beyond (a); the whole log is a second call: `kbf build show <log link> -o <file>` (`GET /v1/blobs/{digest}`, 4.6) |
| (c) The command that ran, to reproduce it | its arguments, the names of its environment variables (never their values), its working directory, its platform properties, its timeout, and the node each attempt ran on | none beyond (a), while the Action and Command blobs are in the CAS (4.7); else the action's link and its `blobs` links |
| (d) Artifacts: list and download | a failed action's outputs inline in (a); every output of the build by the `artifacts` link: each file's path, digest, size, whether its bytes are still held, and its link | a failed action's: none beyond (a); the build's: `kbf build show <artifacts link>`; a download: `kbf build show <output link> -o <file>` |
| (e) Metrics | queue and run times (min, p50, p90, max, sum), reruns by reason, CPU and peak memory per action and summed | `kbf build show <metrics link>` (`GET /v1/invocations/{id}/metrics`, 4.4): one call after (a), or the first if the agent knows the id is good |

Three limits an agent must know, each said again where it applies:

- **The farm sees actions, not the build.** REAPI has no end-of-build call, so the farm
  cannot say the build passed: it says which of the actions it saw failed. buck2's own
  exit code is the build's result; a failure buck2 found without the farm (a parse
  error, a local action) is not in the record.
- **Records do not survive a restart yet.** Until history objects land (`ui-data.md`
  5.2, phase U3, which supersedes PRs 7 and 8), a server restart loses every record, and
  the memory bound evicts idle ones (3.5); with an in-memory object store (tests only) a
  restart loses them after that too, by design. An agent reads soon after its build. A record that is gone
  answers `404` with a body that says whether the server could have held it (4.10).
- **CPU and memory are `null` today** for every action a real driver ran (section 1);
  PR 11 fills them.

### 4.7 The failures view

`GET /v1/invocations/{id}/failures` is what an agent reads first, and what
`kbf build show` reads (4.8). It is a route of its own, not a field of the invocation
answer: its log tails cost a CAS read each and are paged, while the invocation answer
stays one small read of the record that a page or a poll repeats cheaply. Each page
carries the invocation answer of 4.2, its `links` included, so flows (a) to (c) of
[What an agent needs](#what-an-agent-needs) are one call and (d) and (e) are one link
away.

**What it lists**: every action row whose outcome is not `ok` with exit code 0: an OK
status with a non-zero exit, a non-OK status, `timed_out` and `refused`, in first-seen
order (3.2). Failed, timed-out and refused actions are listed together in the order the
farm saw them, not grouped by kind: the earliest is the likeliest cause, and a later
page never reorders an earlier one while the build is still writing. An action still in
flight is not listed; `state: "open"` says more may come. An action that passed after a
rerun is not a failure: its reruns are in its row's attempts and in the metrics (4.4).

```json
GET /v1/invocations/{id}/failures?page_size=20
{
  "invocation": {"id": "0b8c...", "state": "idle", "complete": true,
                 "rows_capped": false, "counts": {"failed": 2, "...": 0},
                 "links": {"failures": "/v1/invocations/0b8c.../failures",
                           "actions": "...", "artifacts": "...", "logs": "...",
                           "metrics": "/v1/invocations/0b8c.../metrics"},
                 "...": "the rest of 4.2"},
  "failures": [
    {"action": "e3b0...-142", "href": "/v1/invocations/0b8c.../actions/e3b0...-142",
     "target": null, "mnemonic": null,
     "outcome": {"ok": {"exit_code": 1}},
     "failure": null,
     "attempts": [
       {"lease": "3.4811", "worker": "node-a",
        "queued": "...Z", "granted": "...Z", "started": "...Z", "ended": "...Z",
        "end": {"requeue": "silent"},
        "run_id": "...", "href": "/v1/runs/...", "...": "the rest of ui-data.md 6.3"},
       {"lease": "3.4812", "worker": "node-b",
        "queued": "...Z", "granted": "...Z", "started": "...Z", "ended": "...Z",
        "end": {"result": "ok"},
        "run_id": "...", "href": "/v1/runs/...", "...": "the rest of ui-data.md 6.3"}],
     "command": {"arguments": ["cc", "-c", "a.c", "-o", "out/a.o"],
                 "environment_names": ["PATH", "LANG"], "working_directory": "",
                 "platform": {"OSFamily": "linux"}, "timeout_micros": null,
                 "output_paths": ["out/a.o"]},
     "stderr": {"digest": "...", "bytes": 10485760, "available": true,
                "tail": "...a.c:3: error: ...\n", "tail_bytes": 16384,
                "truncated": true, "href": "/v1/blobs/...?inline=text"},
     "stdout": null,
     "outputs": [],
     "usage": null}
  ],
  "next_page_token": "...",
  "next_page": "/v1/invocations/0b8c.../failures?page_size=20&page_token=..."
}
```

- **Why it failed**: `outcome` as in 4.2 (a `failed` outcome carries the gRPC status
  and message, `refused` the farm's reason); `failure` is the last attempt's failure
  class (`log.proto` `Failure`: timeout, infra, invalid, out of memory, node memory
  pressure, missing input, lost) with the daemon's reason text, capped at 1 KiB (3.2):
  for an INFRA failure, the farm's own reason (`null`, as here, for an action that ran
  to a non-zero exit). `stdout` or `stderr` is `null` when the result has none.
  `attempts` lists every lease in the attempt shape of `ui-data.md` 6.3 (amended
  there): the built keys of `actions/{digest}` (lease, worker, RFC 3339 `queued`,
  `granted`, `started`, `ended`, `end`), then `run_id` with its `href`, the memory and
  usage fields; queue time is `granted - queued` and run time `ended - granted`, derived
  from those strings, not separate keys. How each ended: the reruns and why.
- **The command** is read when the page is answered, from the Action blob (its
  platform, its timeout, the Command digest) and the Command blob (arguments,
  environment variable names, working directory, output paths). The record keeps
  neither (3.2, section 6), so `command` is `null` and `command_available: false` once
  either blob has left the CAS, or when the client never uploaded the Action. Values
  of environment variables are never put in the answer: they are where a secret most
  often is, and an agent that needs them reads the Command blob by its link (4.2
  `blobs`), which also needs the `commands.getRaw` authority of `ui-data.md` 8
  (never granted by open mode). The node is each attempt's `worker`;
  `GET /v1/nodes` describes it.
- **Outputs**: the action's output files and directories as its row lists them (3.2),
  each with its digest, `available` and `href`, as in 4.2. Only a failed action's
  outputs are inline; every output of the build is the `artifacts` link's, paged,
  because a build of thousands of actions would not fit one answer.
- **Log tails.** For each of stderr and stdout: the last 16 KiB of the blob, cut at the
  byte, decoded as UTF-8 lossily (an invalid or cut sequence becomes U+FFFD), with
  `bytes` the whole log's size and `truncated: true` when the log is longer than the
  tail. The blob is read whole through `Cache::read_blob` and verified before a byte of
  it is used, as on the blob route (4.6: the cache keeps one digest per blob and cannot
  check a part); a log over 64 MiB gets no tail (`tail: null`,
  `tail_omitted: "too_large"`) and is read by its link. A log no longer in the CAS
  says `available: false`, `tail: null`. Each read is a touch.
- **Read budget.** A page reads its logs one at a time, each holding its share of the
  blob route's memory budget (`READ_BUDGET_BYTES`, 1 GiB, in
  `kbf-server/src/http_blobs.rs`) only while it is read and cut, so a page waits for
  at most one log's share at a time. A share not free within that route's wait
  (`READ_BUDGET_WAIT`, 10 s) gives that log `tail: null`,
  `tail_omitted: "busy"`, and the page goes on: the agent reads the log by its link,
  or runs `show` again.
- **Answer budget.** The tails of one page total at most 1 MiB. A page ends before the
  entry whose tails would pass it, and its `next_page` starts at that entry, so an
  entry is on exactly one page and never repeats (4.5). One entry's tails are at most
  32 KiB, so a page always holds at least one entry, and a page may hold fewer entries
  than `page_size`.
- **Who may read**: the `invocations` authorizer of 4.3 for the route; the tails and
  `command` are blob bytes, so they also need `blobs`: with `invocations` and no
  `blobs`, both are `null`, as the `href`s are.
- **Paging**: 4.5's rules; `page_size` default 20, maximum 100. `next_page` is the link
  of the next page, absent on the last. Stage 2 of `invocations.md` 3.5 (a root blob
  with failed-row positions) is superseded by `ui-data.md` 5.2: the `ichunk` index rows
  give each chunk's failed count, and the failed-run section of each chunk with failures
  gives the rows, so only those chunks are read (`ui-data.md` 5.2, 5.3, 5.5).

### 4.8 The client: `kbf build show`

The CEO's ruling (2026-10-12): "I think we should just have show and then that returns
all the info / links to the logs, artifacts, metrics, etc." So the client is one
command, in a new `kbf-cli` crate (one Rust binary, no interpreter or shell), so an
agent never hand-writes HTTP:

```
kbf build show <id or link> [-o <file>] [--server <url>] [--token-file <path>]
```

- **With an id** (an id of 2.2, or a build or action link of 5.2, whose path gives the
  id) it reads the first page of `GET /v1/invocations/{id}/failures` and prints it:
  4.7's answer, in the API's own shape, one call. A failed build's answer holds what
  flows (a) to (c) ask, and the links of (d) and (e). A passing build's answer has an
  empty `failures` and the same links.
- **Absolute links.** The client rewrites every `href`, every `links` value and
  `next_page` from the server's relative path into an absolute URL on the `--server` it
  sent the request to (RFC 3986 reference resolution). The server answers relative
  paths because it does not know the name its clients reach it by; the client does.
  So every link in the answer names the server that already holds the token.
- **With a link** from an answer it reads that link: an API page (an action, the
  artifacts, logs or metrics, the next page of failures) is printed as JSON with its
  links made absolute in the same way; a blob (`/v1/blobs/...`) is written to the file
  `-o` names. The client takes the path and query from the link and sends the request
  to the configured server, never to the link's host, so a link from another source
  cannot carry the token elsewhere.
- **`-o <file>`, the one flag beyond the server and token.** A blob link needs it (a
  blob link without it is a usage error), and an API link may use it. The flag exists
  because without it a download is hand-written HTTP: the token goes in an
  `Authorization: Bearer <token>` header (4.9), and no command line may carry it
  (argv is readable by every user), so the agent would have to build a header file for
  `curl -H @<file>`. The client also checks the blob's SHA-256 against the digest in
  the link, writes to a temporary file in the same directory and renames it into place
  only when the check passes, so a cut or corrupt download leaves no file. It leaves
  out a link's `inline=text`, which changes only the headers and which the route
  refuses (`400`) for a blob over 4 MiB, so a 10 MiB log is still written. Anyone may
  still download a link without the client: it is an ordinary `GET` with that header.
- **JSON always.** An agent is the reader, and it must get the same bytes under a
  pseudo-terminal as under a pipe, so the output does not depend on whether stdout is
  a terminal. A person reads the page of 5.1 or the JSON.
- **Exit status**: 0 the server answered (a build with failures included: the answer
  says so), 3 no record (4.10's body is printed), 5 the route is switched off on that
  server (the `off` body of 4.1 is printed: a different server or policy is the fix,
  not a rerun of the build), 4 refused (`401` or `403`), 2 a usage error, 1 anything
  else (a `503` busy answer included, its body printed). An agent branches on the
  status, not on text.
- **Server and token**: `--server <url>` and `--token-file <path>`; absent, the keys
  `server` and `token_file` of `kbf/client.toml` under the platform's config directory
  (`$XDG_CONFIG_HOME`, default `~/.config`). The token is never on the command line or in
  the config file itself; its file must be the user's alone (mode 0600 or 0400),
  checked as the server checks its own token file (`kbf-server/src/token.rs`). With a
  token, over plain HTTP to an address that is not loopback, the client prints one
  warning line to stderr and goes on (4.9).

### 4.9 Reading from another machine: tokens

Today (section 1) the policy file builds the `allow`, `deny`, `any` and `all`
authentication policies only, and none reads a credential. So an agent on another
machine can read the invocation and blob routes only from a server whose policy allows
everyone who reaches the listener, or through an authenticating proxy the deployment
runs. The requirement, landing with the client tokens of issue 227:

- **A `bearerToken` authentication policy** in the policy file, over 227's client
  token file (`<principal> <role> <qos> sha256:<hex>` lines, digests only, re-read when
  it changes). A request whose `Authorization: Bearer <token>` hashes to a line is
  authenticated as that principal (its `public` metadata: principal and role); no
  header, or no match, is `401` with `WWW-Authenticate: Bearer`. Digests are compared
  in constant time, as for the write token.
- **Scoped read-only**: a `read` role, accepted by the invocation and blob routes and
  refused by every REAPI service and every operator API write. The REAPI refusal is an
  authorizer: a new `kbf-auth` authorizer, `RefuseReadRole`, that answers
  `PERMISSION_DENIED` for a caller whose `public` metadata has `role: "read"` and asks
  the slot's own authorizer otherwise. The server wraps every slot of `Authorizers`
  (GetCapabilities, CAS reads and writes, FindMissingBlobs, GetActionResult, Execute)
  in it when it builds the policy, whatever the file says, so no policy edit can give a
  read token REAPI access. It can, because an `Authorizer` is asked with the caller's
  `AuthenticationMetadata` (`kbf-auth/src/authorize.rs`), not only an instance name.
  Operator API writes already need a loopback peer and the write token, which a read
  token is not: the write token (`--api-token-file`) stays separate, and loopback-only;
  it is not a read credential.
- **Fail closed**: a policy naming `bearerToken` whose token file is missing, readable
  by others or malformed stops the server at start (exit 2); a bad file at a re-read
  refuses every token until a good one is read; without the `invocations` and `blobs`
  keys the routes stay `404`, with the `off` body (4.1, 4.3).
- **TLS stays optional** (ARCHITECTURE.md, security): a token sent in clear can be read
  off the network, so a deployment that serves the API beyond loopback fronts it with
  TLS (a terminating proxy) or runs it on a network that encrypts. The server warns
  and does not refuse, on the bind address alone as for the other listeners: the start
  line's `warning=plaintext-off-loopback:<listeners>` token names `api` too when the
  API listener serves plain text on an address that is not loopback, whatever the
  policy says. 227's step 2 ("accepted only together with TLS") predates that rule; it
  is updated to match.
- `jwt` (planned) and an `invocationOwner` authorizer (4.3) come later; a read token is
  what lets an agent off the server's host read its build now.

### 4.10 A record that is gone

Until history objects land (`ui-data.md` U3, superseding PRs 7 and 8), a restart loses every record and the memory bound evicts idle
ones (3.5): **an agent reads soon after its build.** The server cannot tell a lost id
from one it never saw (in stage 1 it keeps nothing across a restart), so a "lost at
restart" answer would be a guess. It answers `404`, on every route (4.1), with a body
from which the agent can tell:

```json
{"error": "no_record", "id": "0b8c...", "records_since": "2026-10-11T08:00:00Z"}
```

`records_since` is the later of the server's start and the `last_seen` of the newest
client-id invocation the bound has evicted (minted ones do not count: their ids are the
server's own). Every evicted record's `last_seen` is at or before it, so a build whose
calls went on past `records_since` would still be held: its `404` means the farm saw no
call with that id (a build served from buck2's local caches, 5.2, or another server).
A `404` for a build that ended before `records_since` means its record may have been
lost; the agent reruns the build to see it again. The comparison is the agent's: it
knows when its build ended and the server does not, so the client prints the body and
exits 3 in both cases.

**The gap closes with history objects** (`ui-data.md` 5.2 and its phase U3, which
supersede PRs 7 and 8 and the Raft metadata log). A record flushed into an invocation
chunk object survives a restart and the memory bound no longer loses it (a flushed row
leaves memory and is read from its chunk, `ui-data.md` 5.5): what a restart
loses is the history writer's queue and each open invocation's unflushed buffers, and
those invocations say `complete: false` with `gap_from` (`ui-data.md` 5.2), and minted
one-action invocations, which are never flushed. `records_since` then is the oldest time
the history retention classes (`ui-data.md` 5.3) still hold, and a `404` inside it means
the farm saw no call with that id. With an in-memory object store (tests only) a restart
still loses every record, by design, and `records_since` stays the server's start.
