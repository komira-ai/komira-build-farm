# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4). It listens on its
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

Every node registered since the server started, and every node of
[`--expected-nodes`](#expected-nodes) that has not, in node-id order, with its
software and where it is in placement:

```json
{ "nodes": [
  { "node_id": "linux-2", "connected": false, "expected": true,
    "last_seen_unix_ms": null, "software": null,
    "placement": { "state": "absent", "since_unix_ms": 1791369000000 } },
  { "node_id": "mac-1", "connected": true, "expected": true,
    "last_seen_unix_ms": 1791370004000, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "received_at_unix_ms": 1791370000000 },
    "placement": { "state": "draining", "deadline_unix_ms": 1791371800000,
                   "leases": ["117399224320012061.42"] } },
  { "node_id": "old-1", "connected": false, "expected": false,
    "last_seen_unix_ms": 1791369500000, "software": null,
    "placement": { "state": "serving" } }
], "expected_nodes_error": null }
```

| Field | Meaning |
|---|---|
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `expected` | whether `--expected-nodes` lists it |
| `last_seen_unix_ms` | when the server last heard from it (the first `Hello` of its newest stream, or the newest heartbeat taken), by the server's clock; `null` for an `absent` node |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `placement.state` | `absent` (expected, and it has not registered since the server started: the scheduler knows nothing of it); `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`); `drained` (cordoned, no lease left: either its leases ended, or the node disconnected and, after the lease grace, its leases were given up and requeued to run elsewhere; check `connected`); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.since_unix_ms` | while absent: when this server began expecting it (its start, or the reload that first listed it), by the server's clock |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` (each server process has its own term: [worker-protocol.md](design/worker-protocol.md#server-restarts-and-the-lease-epoch)) |
| `expected_nodes_error` (beside `nodes`) | why the newest read of `--expected-nodes` failed (the last list read is still in use); `null` when it succeeded or there is no file |

A node that registered and then disconnected keeps its entry, with `connected: false`,
its `last_seen_unix_ms` and its placement; it is never shown as `absent`.

### Expected nodes

The server keeps what it knows of nodes in memory, so after a restart it lists only
the nodes that have registered since: without help, a node that does not come back
would simply vanish from the list. `--expected-nodes <file>` (which needs
`--api-listen`) names the nodes the server should have; each one that has not
registered since the server started is listed as `absent`, so it can be alerted on.
The file is UTF-8 text of at most 1 MiB. Each line is blank, a `#` comment, or one
node id, as its daemon registers it, optionally followed by a `#` comment:

```text
# the Linux hosts
linux-1
linux-2      # back from repair
mac-07
```

A line of more than one word, or a node listed twice, is refused. The server reads
the file at start and refuses to start if it cannot be read, is not a regular file,
or does not parse. After that, each `GET /v1/nodes` (and each write's answer) takes
the file's metadata and reads it again if its size, device, inode, modification time
or change time changed. A node a reload adds is absent from that reload on; a node it
removes is no longer listed, unless it registered. A reload that fails (the file
removed, unreadable or not parsing) keeps the last list read, so no node is dropped,
and `expected_nodes_error` says why until a read succeeds. Write the file to a new
name and rename it over the old one, so that no read sees half an edit.

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
node that never registered (an `absent` node included) or an unknown verb, `400` for a drain body of another
shape, and `401`, `403` or `415` from the gates of [Who may write](#who-may-write).

A cordon names the node, not its stream: a node that reconnects (say, after a reboot)
is still cordoned. Work that only cordoned nodes could run waits, and its callers are
told so (`every live worker that can run it is cordoned: <nodes>`). It is **not**
refused for it, however long the cordon lasts: a cordon is temporary by intent, and
refusing would fail builds. An uncordon places it at once. The scheduler's unservable
wait (`--unservable-wait-secs`) still refuses work that no connected node, cordoned
or not, could run, and the time spent waiting for a cordon does not count toward it.
A server restart forgets every cordon, as it forgets the rest of the scheduler's
state. The protocol has no drain message yet:
the daemon is not told, and the server simply sends it no new `Start`.

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
