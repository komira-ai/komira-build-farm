# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4). It listens on its
own address, given with `--api-listen`; without the flag there is no API. The start
line then ends with ` api=<addr>`.

**No authentication yet.** Reads answer anyone who reaches the address; a write is
accepted only from a loopback peer (an operator on the server's own host, for example
over SSH) and refused `403` from anywhere else. The roles of
[fleet-updates-security.md](design/fleet-updates-security.md) section S9 (`admin` for
operators, a short-lived `rollout` for CI) are **planned**. Until they land, bind the
API to loopback or an operators-only network.

Every body is `application/json`. What the server keeps here is **in memory**, like
the scheduler's state: a restart forgets it, and each daemon reports again after its
next `Welcome`.

## `GET /v1/nodes`

Every node registered since the server started, in node-id order, with its software
and where it is in placement:

```json
{ "nodes": [
  { "node_id": "mac-1", "connected": true, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "received_at_unix_ms": 1791370000000 },
    "placement": { "state": "draining", "deadline_unix_ms": 1791371800000,
                   "leases": ["1.42"] } },
  { "node_id": "old-1", "connected": false, "software": null,
    "placement": { "state": "serving" } }
] }
```

| Field | Meaning |
|---|---|
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `placement.state` | `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`); `drained` (cordoned, no lease left); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` |

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
shape, `403` from a peer that is not loopback.

A cordon names the node, not its stream: a node that reconnects (say, after a reboot)
is still cordoned. Work that only cordoned nodes could run waits, and its callers are
told so; like work no connected node can run, it is refused after the scheduler's
unservable wait (`--unservable-wait-secs`). A server restart forgets every cordon, as
it forgets the rest of the scheduler's state. The protocol has no drain message yet:
the daemon is not told, and the server simply sends it no new `Start`.

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
