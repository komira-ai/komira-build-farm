# Operator verbs: every verb, the route that does it, or why it stays local

Status: a reference table. Every route of `kbf-server` must have a row here by method and
path, and the node verbs (`:cordon`, `:drain`, `:uncordon`) one row each:
`crates/kbf-server/tests/route_docs.rs` fails otherwise (it does not check the gate column). The authority for
a built route is [../api.md](../api.md); for an open pull request, that pull request. This page
is the index: the verb an operator means, the one route that does it, and the gate it passes.

Labels: **[F]** read in code on this branch, **[V]** an open pull request, not on the branch,
**[N]** new in the object-store series ([object-store-registration.md](object-store-registration.md)),
**[D]** a decision.

The rule (distribution.md section 1): everything configurable is an API call first; `curl` is
the contract; the `kbf` CLI is a typed wrapper with no logic of its own. A verb that is not a
route stays local, and the last column says why.

**Gates** (the first column of routes below; the text is [../api.md](../api.md), "Who may write"):

- **open**: a read, answered to whoever reaches the address.
- **write**: loopback peer, no `Origin`, the token file, a Bearer token, and `Content-Type:
  application/json`. A release upload drops the content-type gate.
- **full**: the write gate on a read too, and `Cache-Control: no-store`.
- **policy**: the REAPI authentication policy and `invocations.getAuthorizer`; off (404) without
  them.
- **none**: health and metrics, which need no credentials.

## 1. Routes, one row each

Every row is one method and one path. A router `.route(` call that serves two methods has a row
per method; a custom method (`:cordon`) has a row per verb. The sources of the router are
`crates/kbf-server/src/{api,release/http,storage_health,health,metrics,http_blobs,
http_invocations,http_sched,http_history,enroll/api,node_self,ui_mount}.rs` and `frontend.rs`, where they are merged.

### Cluster

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| cluster status (nodes, software, placement) | `GET /v1/nodes` | [F] | open |
| one node (capacity, booked, free, leases) | `GET /v1/nodes/{node}` | [F] | open |
| a storage host's own member and the storage numbers (its updater, on the worker listener) | `GET /v1/nodes/{node}/storage-health` | [F] | the `updater` certificate of that node, over mutual TLS ([../api.md](../api.md), "Own-node calls of a host's updater") |
| list members of the storage map | `GET /v1/members` | [F] | open |
| leases the scheduler holds (paged, by node) | `GET /v1/leases` | [F] | open |
| the queue: depth, ages, why work waits | `GET /v1/queue` | [F] | open |
| cordon a node | `POST /v1/nodes/{node}:cordon` | [F] | write |
| drain a node | `POST /v1/nodes/{node}:drain` | [F] | write |
| uncordon a node | `POST /v1/nodes/{node}:uncordon` | [F] | write |
| set a member active or draining (join, leave) | `POST /v1/members/{id}` | [F] | write |
| may a storage member's host be stopped now (the update gate) | `GET /v1/members/{id}/safe-to-stop` | [F] | write |
| deny an entry (serial, SPKI hash, node id) | `POST /v1/denied` | [F] | write |
| allow an entry again | `DELETE /v1/denied` | [F] | write |
| list the denied entries | `GET /v1/denied` | [N] unit F | write |
| frontend drain status | `GET /v1/frontend/drain` | [F] | open |
| begin the frontend's drain | `POST /v1/frontend/drain` | [F] | write |
| frontend retire status | `GET /v1/frontend/retire` | [F] | open |
| retire the frontend (stop its intake and worker streams, keep its callers) | `POST /v1/frontend/retire` | [F] | write |
| raise the stored epoch (after a restore from a backup) | `POST /v1/epoch/bump` | [F] | write |
| storage health (under-replicated blobs) | `GET /v1/storage` | [F] | open |

There is no rebalance verb: handoff of data between members is not built, and none is invented
here **[D]**.

### Admin port (removed)

A frontend serves no admin port: `GET /v1/admin/role` and the `quiesce`, `resume`, `takeover`
and `stand-down` posts are gone with `--admin-listen` ([../api.md](../api.md), "The admin calls
(removed)").

### Enrollment

All of these answer 404 until enrollment is wired into the frontend (#1055 **[V]**).

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| create an enrollment token | `POST /v1/enroll/tokens` | [F] | full |
| list tokens | `GET /v1/enroll/tokens` | [F] | full |
| revoke a token | `DELETE /v1/enroll/tokens/{id}` | [F] | full |
| list enrolled nodes | `GET /v1/enroll/nodes` | [F] | full |
| revoke an enrolled node | `POST /v1/enroll/nodes/{id}/revoke` | [F] | full |

### Object store

Designed in [object-store-registration.md](object-store-registration.md); unit C unless noted.
All answer 404 when the feature is off.

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| register or replace a connection | `PUT /v1/object-stores/connections/{name}` | [N] | full |
| list connections | `GET /v1/object-stores/connections` | [N] | full |
| show a connection | `GET /v1/object-stores/connections/{name}` | [N] | full |
| delete a connection | `DELETE /v1/object-stores/connections/{name}` | [N] | full |
| probe a connection | `POST /v1/object-stores/connections/{name}:verify` | [N] | full |
| bind a role | `PUT /v1/object-stores/bindings/{role}` | [N] | full |
| unbind a role | `DELETE /v1/object-stores/bindings/{role}` | [N] | full |
| desired state and convergence | `GET /v1/object-stores` | [N] (convergence from unit D) | full |
| audit log | `GET /v1/object-stores/audit` | [N] | full |

### Releases and updates

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| the frontend's own update order, read | `GET /v1/update/frontend` | [V] #1062 | open |
| the frontend's own update order, write | `POST /v1/update/frontend` | [V] #1062 | write |

The server keeps no desired-release routes (`GET /v1/update`, `POST /v1/update/desired`,
`pause`, `resume`): updates are OS packages installed by `kbf-updater`. There is no image-pull
verb and no CA-rotation route on this branch: both are out of scope of the object-store series (Q5).

### Reads of builds

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| a build (invocation) | `GET /v1/invocations/{id}` | [F] | policy |
| its action rows | `GET /v1/invocations/{id}/actions` | [F] | policy |
| one action's row | `GET /v1/invocations/{id}/actions/{digest}` | [F] | policy |
| its artifacts | `GET /v1/invocations/{id}/artifacts` | [F] | policy |
| its logs | `GET /v1/invocations/{id}/logs` | [F] | policy |
| its metrics | `GET /v1/invocations/{id}/metrics` | [F] | policy |
| its failed actions, with reasons, commands and log tails | `GET /v1/invocations/{id}/failures` | [F] | policy |
| list invocations | `GET /v1/invocations` | [F] | policy |
| list shapes | `GET /v1/shapes` | [F] | policy |
| list tests | `GET /v1/tests` | [F] | policy |
| one shape | `GET /v1/shapes/{key}` | [F] | policy |
| a shape's recent runs | `GET /v1/shapes/{key}/runs` | [F] | policy |
| one run | `GET /v1/runs/{id}` | [F] | policy |
| a run's logs | `GET /v1/runs/{id}/logs/{stream}` | [F] | policy |
| a blob's bytes | `GET /v1/blobs/{digest}` | [F] | policy |
| outcomes of shapes across builds | `GET /v1/grid` | [V] #891 | policy |
| runs of an action digest | `GET /v1/actions/{digest}/runs` | [V] #891 | policy |
| runs on a node in a window | `GET /v1/runs` | [V] #891 | policy |

### The web UI

The pages of `kbf-ui`, read-only HTML under `/ui` ([api.md](../api.md#get-ui-the-web-ui)); on by
default, `--ui=false` turns them off. Open: no gate, whatever the REAPI policy says.

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| the Overview page | `GET /ui` | [F] | none (open) |
| the same, with a trailing slash | `GET /ui/` | [F] | none (open) |
| every other page and the stylesheet (`/ui/nodes`, `/ui/nodes/{id}`, `/ui/builds`, `/ui/builds/{id}`, `/ui/leases`, `/ui/queue`, `/ui/static/kbf.css`) | `GET /ui/{*rest}` | [F] | none (open) |

### Health

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| liveness | `GET /healthz` | [F] | none |
| readiness | `GET /readyz` | [F] | none |
| farm summary (ready, nodes by state, queue, leases, storage) | `GET /v1/health` | [F] | open |
| metrics | `GET /metrics` | [F] | none |

The Mac device manager is a separate service, not the operator API (`crates/kbf-mdm`: `GET
/v1/macs`, `GET /v1/macs/{serial}`, `POST` on `.../enforce`, `.../withdraw`, `.../profile`,
`.../grant-admin`, `.../erase` and `.../bring-forward`). It leaves the product (distribution.md
section 8) and is not an operator verb of the farm.

## 2. Verbs that are not routes

| Verb | Where it lives | Stays local because |
|---|---|---|
| apply a registered object store on a storage node | the node's reconcile at start (unit E) | no operator verb: the node reconciles by itself; in v1 a rotation reaches a running node at its restart |
| restart a node so it picks up a rotation | the installer (`kbf-installer`, #1048, #1051) | kbf never restarts itself; the installer verb for it does not exist yet, so a rotation ends with the operator waiting on `pending` (a cluster write, held) |
| frontend state backup | none: Postgres is backed up by the installer | no route and no flag |
| meta-backup, meta-verify | `kbf-deploy linux` **[F]** | briefly stops `kbf-server`; installer or root only |
| install, update, roll back, confirm, revert, gc, status and check of a host | `kbf-installer` (#1048, #1051, #1082, #1083) **[V]** | root and host-local; the installer is the root tool, and `setup` mints the first token |
| the rollout controller | `kbf-deploy rollout`, #803, #970 **[V]** | a host state machine; installer territory |
| start a mode, `--check` | `kbf <mode>` **[F]** | the local runtime; it installs nothing |
| frontend handover, enroll, `kbf node` | `kbf frontend handover`, `kbf enroll`, `kbf node` **[F]** | removed: a frontend is updated by a roll of the `kbf-proxy` pool, driven by `kbf-updater` (`frontend-proxy.md` section 4) **[V]** |
| render, check, install units, swap, verify certificate (the `envoy render, check, apply` verbs are retired: no Envoy) | `kbf-deploy` **[F]** | become installer logic and package units; root |
| `mdm`, `minio`, `pilot`, `erase-request` | `kbf-deploy` **[F]** | leave the product (distribution.md section 8); an erase request is signed with a security key on the operator's machine |
| `kbf cluster status`, `drain`, `undrain`, `member`, `denied`, `epoch-bump`, `object-store status\|audit\|connection\|bind\|unbind\|verify` | the `kbf` CLI **[F]** (the last two groups: unit G1) | a thin client of the routes above, no logic |
| frontend log tail | none | cut (Q4) |
| update policy, image pull | none | no source verb exists (Q5) |
| rotate the CA | its own unit | out of the object-store series (Q5) |
