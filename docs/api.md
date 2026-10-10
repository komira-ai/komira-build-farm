# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4), and
[`/healthz` and `/readyz`](#get-healthz-and-get-readyz) for a front's health check (the REAPI
listener answers the same readiness over [`grpc.health.v1`](#grpchealthv1-on-the-reapi-listener)). It listens on its
own address, given with `--api-listen`; without the flag there is no API. The start
line then ends with ` api=<addr>`. The listener speaks HTTP/1.1, and HTTP/2 in clear
text (h2c, with prior knowledge); every rule below holds for both.

Reads answer anyone who reaches the address: bind it where only operators do. Writes
need a token (see [Who may write](#who-may-write)). The roles of
[fleet-updates-security.md](design/fleet-updates-security.md) section S9 (`admin` for
operators, a short-lived `rollout` for CI) are **planned**; the token is the one
operator credential until they land.

Every body is `application/json`. What the server keeps here is **in memory**, like
the scheduler's state: a restart forgets it, and each daemon reports again after its
next `Welcome`.

## Who may write

A write (cordon, drain, uncordon) can take every node in the fleet out of service, so
it must come from an operator. Where a request comes from does not show that:

- **Every server host is also a worker.** Build actions run on it, and they can reach
  loopback: the native driver on Linux enforces no network isolation, and the macOS
  sandbox allows loopback.
- **A reverse proxy on the host** (`tailscale serve`, nginx) makes every caller it
  forwards a loopback peer.
- **A browser on the host** can POST to a loopback address from any page it opens,
  without a preflight, if the API accepts a "simple" request (`text/plain` or a form
  body, or no body).

So a write must pass every one of these gates, in this order:

| Gate | Refused with |
|---|---|
| the peer is loopback, so the token never crosses a network in clear | `403` |
| the request has no `Origin` header (browsers send one with a cross-origin POST) | `403` |
| the server was started with `--api-token-file` (without it, writes are off) | `403` |
| `Authorization: Bearer <token>` with the file's token (the SHA-256 digests of both are compared in constant time, so neither the token's bytes nor its length show through timing) | `401`, with `WWW-Authenticate: Bearer` |
| `Content-Type: application/json` (parameters allowed), which a page cannot send cross-origin without a preflight the API never answers | `415` |

**The token file.** `--api-token-file <path>` names a file holding one token of at
least 32 visible ASCII characters (surrounding whitespace, such as the trailing
newline, is dropped), in at most 4096 bytes; `openssl rand -hex 32` makes one. The
file must be a regular file (a FIFO or device is refused at once, without waiting on
it) owned by the server's user with mode exactly `0600` or `0400`; otherwise the
server refuses to start. The token is read once at start: it is never on the command
line, in the start line or in a log, and a change to the file takes effect at the
next start.

**Run `kbf-server` as its own user.** Keep the token out of the build actions'
reach: a build action that can read the file can write. On a host that is also a
worker, `kbf-server` must run as a different user from `kbf-daemon` and from every
user leases run as. The native driver runs actions as the daemon's user, so a `0600`
token file owned by a user the server shares with the daemon is readable by every
build on that host.

**Through a reverse proxy.** A remote operator reaches the API through a proxy on the
server's host that terminates TLS (`tailscale serve`, nginx). The proxy is what makes
the loopback gate pass, so the token is what stands between the network and the
writes: forward the `Authorization` header unchanged, and expose only what the
operators' network should reach. A proxy that adds an `Origin` header to every
request turns writes off.

## `GET /v1/nodes`

The server that answers, then every node registered since it started, in node-id
order, with its software and where it is in placement:

```json
{ "server": { "version": "0.1.0+0123456789ab", "commit": "0123456789ab" },
  "nodes": [
  { "node_id": "mac-1", "connected": true, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "xcodes": [
        { "app": "/Applications/Xcode_15.4.app", "build": "15F31d", "state": "ready",
          "reason": "", "fix": "" },
        { "app": "/Applications/Xcode_16.1.app", "build": "16B40",
          "state": "license_not_accepted",
          "reason": "/usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.",
          "fix": "sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" },
        { "app": "/Applications/Xcode_16.2.app", "build": "16C5032a", "state": "ready",
          "reason": "", "fix": "" } ],
      "received_at_unix_ms": 1791370000000 },
    "needs_attention": [
      "Xcode 16B40 (/Applications/Xcode_16.1.app) installed but not ready: /usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.; fix: sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" ],
    "placement": { "state": "draining", "deadline_unix_ms": 1791371800000,
                   "leases": ["117399224320012061.42"] } },
  { "node_id": "old-1", "connected": false, "software": null, "needs_attention": [],
    "placement": { "state": "serving" } }
] }
```

| Field | Meaning |
|---|---|
| `server.version` | the `kbf-server` build that answers, as `--version` and the start line print it: the package version, `+`, and the commit |
| `server.commit` | the commit it was built from, 12 hex digits (`git rev-parse --short=12`), or `unknown` for a build without a git checkout |
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `software.xcode_builds` | the Xcode builds actions can use on it: only these route work |
| `software.xcodes` | every installed Xcode, ready or not: `app`, `build` (empty if not known), `state` (`ready`, `license_not_accepted`, `first_launch_not_run`, `metal_toolchain_missing`, `failed`, `not_surveyed` from the daemon's start until its first survey of its Xcodes ends (never advertised; the daemon sends its status again when the survey ends), or `unknown` for a state this server does not know), `reason` (the check that failed and its answer, from the daemon's last survey that changed this Xcode's `DEVELOPER_DIR`, build or state; a survey that differs only in its reason is not sent, so when another check later fails with the same build and state, this is still the earlier check's text) and `fix` (the command an administrator runs on the node, when one is known). Empty from a daemon that predates it |
| `needs_attention` | what an operator must do on the node, one line per item: today each installed Xcode that is not ready, as `Xcode <build> (<app>) installed but not ready: <reason>; fix: <command>`. An Xcode `not_surveyed` keeps the item the same app had in the node's previous status (as when a restarted daemon sends its first status), until a survey reports it; one with no previous item has none. The server also logs each item once at `WARN` (target `kbf_server::attention`) when it appears, and at `INFO` when a status reports it gone (its Xcode ready, changed or removed); a status that repeats an item, or lists its Xcode `not_surveyed`, logs nothing. An item is the same while its Xcode's app, build, state and fix are, so a reason that changes alone is shown here but not logged again. The previous status is kept in the server's memory: after a server restart, each item is logged again when its node reports it; kbf has no alert delivery yet (issue #189) |
| `placement.state` | `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`); `drained` (cordoned, no lease left: either its leases ended, or the node disconnected and, after the lease grace, its leases were given up and requeued to run elsewhere; check `connected`); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` (each server process has its own term: [worker-protocol.md](design/worker-protocol.md#server-restarts-and-the-lease-epoch)) |

## `POST /v1/nodes/{node}:cordon`, `:drain`, `:uncordon`

An operator takes a node out of placement, drains it, or returns it
([fleet-updates.md](design/fleet-updates.md) sections 3.3 and 4.2):

- `:cordon`: the scheduler offers the node no new lease; its leases run on. A drain
  already under way is kept.
- `:drain`: cordons the node and waits for its leases to end. The body may be
  `{"deadline_secs": N}`; without a body the deadline is 30 minutes. With no lease
  left the node is `drained`. If the deadline comes first the drain **pauses**: the
  leases run on, nothing is killed or given up, and the node stays `drain_paused`
  even after they end, until an operator drains it again (a new deadline) or
  uncordons it.
- `:uncordon`: returns the node to placement and ends any drain.

The answer is the node, as `GET /v1/nodes` lists it, after the action. `404` for a
node that never registered or an unknown verb, `400` for a drain body of another
shape, and `401`, `403` or `415` from the gates of [Who may write](#who-may-write).

A cordon names the node, not its stream: a node that reconnects (say, after a reboot)
is still cordoned. Work that only cordoned nodes could run waits, and its callers are
told so (`every live worker that can run it is cordoned: <nodes>`). It is **not**
refused for it, however long the cordon lasts: a cordon is temporary by intent, and
refusing would fail builds. An uncordon places it at once. The scheduler's unservable
wait (`--unservable-wait-secs`) still refuses work that no connected node, cordoned
or not, could run, and the time spent waiting for a cordon does not count toward it.
A server restart forgets every cordon, as it forgets the rest of the scheduler's
state, and every start logs a warning that says so. The protocol has no drain
message yet: the daemon is not told, and the server simply sends it no new `Start`.

## `GET /healthz` and `GET /readyz`

For the health check of a front or proxy (a TLS-terminating ingress, Envoy) in front
of the REAPI listener. Both are reads, open like `GET /v1/nodes`, and are served only
when `--api-listen` is given. The routes are served by the same task as the REAPI
and worker listeners, which are bound before it starts and stop when it ends, so any
answer means both are bound.

**`GET /healthz`**: the process is alive. It reads nothing, and answers `200` for as
long as the server runs, during a stop too:

```json
{ "status": "alive", "version": "0.1.0+0123456789ab", "commit": "0123456789ab" }
```

**`GET /readyz`**: the server is ready to serve. `200` when every check passes, `503`
otherwise; the body lists each failing check and why:

```json
{ "ready": false, "version": "0.1.0+0123456789ab", "commit": "0123456789ab",
  "failing": [ { "check": "store",
                 "reason": "a read of kbf/1791370000000000000/readyz-probe had no answer within 2000 ms" } ] }
```

| Check | Fails when |
|---|---|
| `stopping` | the server has received SIGTERM or SIGINT. It answers `503` from that moment, before its REAPI streams are ended, and for the rest of its drain (`--shutdown-timeout-secs`) |
| `leader` | the server does not hold the scheduler role. A single server runs every role and always holds it; the check is there for a replicated control log, whose followers fail it. **Planned**: a second readiness path, without this check, for the servers that may serve reads ([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)) |
| `store` | the object store does not answer a one-byte read of the key `<prefix>readyz-probe` within `--readyz-store-timeout-ms` (default 2000; it bounds the gRPC health service's probe too), or answers with an error. The probe only reads: no poll writes, deletes or lists. Nothing writes that key, so "not found" is the expected answer and passes, as does the object's bytes or a range past its end |

`version` and `commit` are those of `GET /v1/nodes`' `server` field. Each `/readyz`
makes one read of the store, so poll it at the interval the front needs, not faster.

## `grpc.health.v1` on the REAPI listener

For a proxy that health-checks its backends over gRPC (Envoy's `grpc_health_check`, a
gRPC load balancer), the REAPI listener (`--listen`) serves the standard
[gRPC health checking protocol](https://github.com/grpc/grpc-proto/blob/master/grpc/health/v1/health.proto),
`grpc.health.v1.Health`, whether or not `--api-listen` is given, and over the
listener's TLS when `--reapi-tls-cert` and `--reapi-tls-key` are given. Its answer is
`/readyz`'s: `SERVING` when every check of the [table above](#get-healthz-and-get-readyz)
passes, `NOT_SERVING` when one fails. It gives no reason; `/readyz` names the failing
checks.

It answers for these service names, all with the same status: `""` (the server as a
whole), `build.bazel.remote.execution.v2.Capabilities`,
`build.bazel.remote.execution.v2.ContentAddressableStorage`,
`google.bytestream.ByteStream`, `build.bazel.remote.execution.v2.ActionCache` and
`build.bazel.remote.execution.v2.Execution`.

| Method | Answer |
|---|---|
| `Check` | the status, after one store probe (as one `/readyz`). Another service name is `NOT_FOUND` |
| `List` | every name above with the status, after one store probe |
| `Watch` | the status at once, then each time it changes. It evaluates again as soon as a stop signal arrives or the scheduler role changes, and probes the store every 5 s. Once the server is stopping it sends `NOT_SERVING` (if that was not its last message) and ends the stream `UNAVAILABLE`, so an open Watch does not hold up the drain. Another service name is sent `SERVICE_UNKNOWN`, and the stream stays open until the server stops |

On SIGTERM or SIGINT the status turns `NOT_SERVING` before the REAPI streams are
ended and the listener stops accepting. After that the listener takes no new calls,
so a `Check` during the drain fails or times out instead of answering; either way the
proxy marks the server down.

The REAPI authentication policy (`--reapi-auth-policy`, [reapi-auth.md](reapi-auth.md))
does not run on these calls: a health check needs no credentials, whatever the policy.
Every other call on the listener, a path it does not serve included, still runs it.
The store probe waits at most `--readyz-store-timeout-ms`, like `/readyz`'s.

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
