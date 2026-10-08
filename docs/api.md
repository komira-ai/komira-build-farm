# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4). It listens on its
own address, given with `--api-listen`; without the flag there is no API. The start
line then ends with ` api=<addr>`.

**No authentication yet.** The API answers anyone who reaches its address. The roles
of [fleet-updates-security.md](design/fleet-updates-security.md) section S9 (`admin`
for operators, a short-lived `rollout` for CI) are **planned**. Until they land, bind
the API to loopback or an operators-only network.

Every body is `application/json`. What the server keeps here is **in memory**, like
the scheduler's state: a restart forgets it, and each daemon reports again after its
next `Welcome`.

## `GET /v1/nodes`

Every node registered since the server started, in node-id order:

```json
{ "nodes": [
  { "node_id": "mac-1", "connected": true, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "received_at_unix_ms": 1791370000000 } },
  { "node_id": "old-1", "connected": false, "software": null }
] }
```

| Field | Meaning |
|---|---|
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
