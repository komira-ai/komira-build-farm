# kbf-updater: rolling a version over a farm

`kbf-updater` rolls one version of the `kbf` package over the hosts of a farm, one host at a
time, behind kbf-proxy's local socket. It runs on the frontend host. It installs through the
forced command of [`kbf-host-install`](host-install.md) over ssh, asks the frontend's loopback
operator API ([the API](../api.md)) whether a host may be stopped and whether it runs the target,
and writes its progress to `roll.json` in its state directory before each step, so a restart
of the updater finds a roll where it was.

## The inventory

`--inventory` names a root-owned file (and a root-owned directory), one host per line:

```text
# alias  address               role        host key (type and base64)                 [node id]
fe-a     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=a base=18000
fe-b     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=b base=18010
st1      st1.farm.example      storage     ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
dm1      dm1.farm.example      daemon      ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  dm1
```

The roles are `frontend`, `storage`, `daemon` and `mac-daemon`. A daemon host lists the node id
of its kbf configuration as the sixth word.

### A frontend that is a member of the kbf-proxy pool

A `frontend` line with two more words after the key is one member of a pool that kbf-proxy
balances over (pool mode, the `[[proxy.frontend]]` tables of [the proxy's API](../design/kbf-proxy-api.md)):

| word | meaning |
|---|---|
| `instance=<name>` | the name the proxy knows the member by (`name` of its `[[proxy.frontend]]` table) and the unit instance the host restarts for it: `kbf-frontend@<name>.service`. `-` is the plain unit `kbf-frontend.service`. Otherwise `[a-z][a-z0-9-]{0,31}` |
| `base=<port>` | the first of the member's five ports (`base_port` of the table), 1024 to 65531. The member's operator API is at `base` plus 2 on loopback |

- There is one line per member, and every line lists the same address and host key: every
  member runs on the host the updater runs on, and the retire and the commit go to the member
  on loopback while the install and the restart go over ssh to the address, so a member that
  lists another address or host key than an earlier one is refused.
- No two members share an `instance`, and their bases are at least 5 apart (a member uses
  `base` to `base` plus 4), or the file is refused.
- A pool of members and a `frontend` line without these words cannot be in one file, and a
  second `frontend` line without them is refused as before. A `frontend` line without them is
  not in a pool, nothing can move traffic away from it, and a roll that reaches it fails
  there (`swap_abandoned`) before any ssh call.
- `instance=` and `base=` are accepted on a `frontend` line only.

## The flags

All are flags; none is read from the environment.

| flag | |
|---|---|
| `--state-dir`, `--inventory`, `--proxy-uid`, `--token-file`, `--ssh-key-file`, `--ssh-user`, `--channel` | required |
| `--operator-api http://<loopback ip>:<port>` | required: the frontend's operator API, for safe-to-stop and the health of storage and daemon hosts. Not a loopback address is refused |
| `--operator-proxy-auth-file` | the proxy's per-boot secret, for a frontend that runs behind kbf-proxy |
| `--proxy-admin http://<loopback ip>:<port>` | optional: the admin API of the kbf-proxy in pool mode (`admin_listen`). Given, a pool member is rolled through the proxy ([below](#what-a-roll-does-with-a-frontend-pool-member)); not given, it is rolled without one ([below](#a-frontend-pool-member-without-a-proxy)). Refused unless it is a loopback address. It asks for no credential, and none is sent |
| `--socket` | the roll API's socket, for tests and development; in production systemd passes it |
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--drain-deadline-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 600, 5 and 600 |
| `--peer-ready-secs`, `--frontend-drain-deadline-secs` | optional bounds in seconds, at least 1, for a member rolled without a proxy: how long every other member must answer `/readyz` 200 in a row before a member is drained (default 30), and how long a member is given to exit once it is told to drain (default 1200, which must be longer than the longest execution a member may have open). Nothing reads them when `--proxy-admin` is given |

The updater reaches each member's operator API on loopback at `127.0.0.1:<base + 2>`, with the
same token (`--token-file`) and proxy secret as `--operator-api`.

## What a roll does with a frontend pool member

A roll takes the members before every other host, one member at a time, and does not begin one
until the one before it is back in the pool. For a member the turn is:

1. **Check.** The proxy's list (`GET /v1/proxy/upstreams`) must show some other member up and
   ready. If none is, the roll fails with `unsafe_to_stop` and nothing has been changed: the last
   member that is up is never taken out. The member itself need not be up: one that is drained
   or not ready is taken out of nothing, and is rolled while one other member serves.
   (`unknown_member` if the proxy does not list the member, `safe_to_stop_unavailable` if the
   list cannot be read.)
2. **Drain** (`draining`). `POST /v1/proxy/upstreams/<instance>:drain`: the proxy gives the member
   no new stream. The roll then waits until the REAPI and operator streams the proxy shows open
   on the member are 0, for at most 900 seconds (`drain_failed` if they are not).
3. **Retire** (`retiring`). `POST /v1/frontend/retire` on the member's operator API: the member
   hands its work to the other frontends, and the roll waits until the member reports no stream
   open, for at most 900 seconds (`retire_failed`). The call is answered when the retire has
   ended, so it is given 120 seconds to be answered.
4. **Install** (`installing`). The package is installed on the host through `kbf-host-install`,
   unless the host already has the target. An install the host reports as in progress is waited
   for, for at most 900 seconds (`install_failed`), and not run a second time beside it. The
   package covers every member of the host, so this happens once per host, at the first member.
5. **Restart** (`restarting`). `restart frontend=<instance>` through `kbf-host-install`
   (`restart_failed`).
6. **Ready** (`waiting_healthy`). The roll waits until the proxy lists the member ready and the
   member's `GET /healthz` reports a `commit` that the target's version names, for at most the
   frontend health bound, 300 seconds (`health_timeout`; `swap_abandoned` if the version names no
   commit or the proxy drops the member).
7. **Undrain** (`undraining`). `POST /v1/proxy/upstreams/<instance>:undrain` (`undrain_failed`).

Every failure after the check stops the roll. One after the proxy has drained the member **leaves
it drained**: the detail names it (`frontend member a (fe-a) is left drained`). A drain the proxy
refuses says only that, and the member may not be drained. The updater never undrains a member it
could not show to be healthy. An operator looks at the member, and either fixes it and starts
another roll (a member found drained is rolled again as long as another member is up) or puts it
back by hand:

- A failure before the retire (the check, the drain, the wait for streams) leaves a member that
  still serves its work as before, and `POST /v1/proxy/upstreams/<instance>:undrain` puts it back.
- A failure at the retire or after it may leave a member that is **retired**: it refuses new work
  and is not ready until its unit is restarted, and undraining it would put a member that does not
  serve back in the pool. Restart the unit (`restart frontend=<instance>` of `kbf-host-install`)
  or start another roll, which finishes the member. Undrain by hand only once the member is ready
  and runs the target.

### A restart of the updater

Each phase above is written to `roll.json` before its work begins. A roll found `running` after
a restart is resumed with `drive`: the member in hand is drained again (a drain does nothing to a
member that is already out), and the turn goes on from the phase the record holds. A member found
in `installing` asks the host what it has and installs only if it does not have the target
(waiting out an install it reports as in progress); one found in `undraining` is drained again
and put back.

A `roll.json` written by an earlier build, which has none of the phases `draining`, `retiring`,
`restarting` and `undraining`, is read as it was: no field was added, and the format is still 1.
The other way does not hold: an earlier build does not know these phases and refuses to start on
a `roll.json` that holds one, so finish or abort a roll that is in a member's turn before
downgrading the updater.

## A frontend pool member without a proxy

A farm whose frontends sit behind a load balancer of its own (Envoy, a cloud balancer, anything
that stops sending to a backend whose `/readyz` answers 503) needs no kbf-proxy to be rolled.
Start the updater **without `--proxy-admin`** and the inventory lines with `instance=` and `base=`
are rolled by the member's own drain. The load balancer is never asked: the member says it is not
ready, and the balancer, whatever it is, stops sending to it. The member is rolled alone, and the
turn is:

1. **Check** (`pending`). Every other member of the inventory is asked `GET /readyz` on its own
   operator port (`base` plus 2) every 5 seconds. Each must answer 200 every time for
   `--peer-ready-secs` (30) in a row; an answer of 503, and a member that cannot be asked, start
   the window again. If the window has not been had `--peer-ready-secs` plus 300 seconds after the
   first ask, the roll fails with `unsafe_to_stop` and nothing has been changed. A member that is
   the only one in the inventory has no other to serve and is refused the same way. The member
   itself is not asked.
2. **Drain** (`draining`). `POST /v1/frontend/drain` on the member's operator API. The frontend
   then runs its own drain, which does not depend on the balancer: `/readyz` 503, no new work and
   GOAWAY on its listeners, a wait for the executions open on it, and its exit. The roll waits
   until the member's operator API refuses the connection (it has exited) or its
   `GET /v1/frontend/drain` reports `"closing": true` (the frontend in this tree does not report
   it, and is waited for until it exits), for at most `--frontend-drain-deadline-secs`
   (`drain_failed`). A member that answers the status but says it is not draining fails the roll
   at once (`drain_failed`). A member that is not running refuses the connection of the drain
   itself, has nothing to drain, and the turn goes on; a drain that gets no answer (a timeout)
   fails the roll with `frontend member a (fe-a) may be draining`.
3. **Install** (`installing`). As with a proxy: the package is installed on the host unless it has
   the target, once per host, at the first member.
4. **Restart** (`restarting`). `restart frontend=<instance>` through `kbf-host-install`, which
   starts the new version (`restart_failed`).
5. **Ready** (`waiting_healthy`). The roll waits until the member's own `GET /readyz` answers 200
   and its `GET /healthz` reports a `commit` that the target's version names, for at most 300
   seconds (`health_timeout`; `swap_abandoned` if the version names no commit).

Nothing is put back at the end: a drained frontend has exited, and the restart made a new process,
which is ready or is not. The next member is rolled only when this one is ready, which is also
what the next check asks of it.

**The bound.** `--frontend-drain-deadline-secs` (default 1200) must be longer than the longest
execution a member may have open, or the roll gives up on a drain that is still going as it
should. The frontend in this tree puts no bound of its own on a drain that its endpoint began
(`--shutdown-timeout-secs` bounds only a drain that a signal began), so an execution that runs
past the bound fails every roll at that member until it ends. A drain that outlasts the bound
fails the roll with `drain_failed` and `frontend member a (fe-a) is left draining`: the member
goes on draining and exits by itself, and a new roll finds it stopped (its connection is refused)
and goes on from the install.

**A member that is stopped, or a wrong `base`.** A member whose operator API refuses the
connection of the first drain is taken for one that an earlier roll left stopped, and is installed
on and restarted. A `base` in the inventory that is not the member's has the same look: the unit is
restarted though it was never drained, which ends any execution open on it (a Buck2 build does not
reattach, so each is a failed action), and the wait for readiness fails, since it asks at the same
wrong address. Check the `base` of a new line before the first roll.

Every failure after the drain stops the roll and names the member as not serving (`frontend member
a (fe-a) is not serving`): the install, the restart and the readiness wait leave a member that is
down or not ready, and the load balancer sends nothing to it. The other members serve. An operator
fixes it and starts another roll, which begins at the check again.

A restart of the updater finds the phase in `roll.json`. In `draining` the member's drain is read
first: a member that says it is not draining was never told to (the record is written before the
drain is sent), and is checked against the other members as in the check. The member is then told
to drain again (a drain does nothing to a member that is draining) and its exit is waited for; in
`installing` the host is asked what it has, as with a proxy; in `restarting` and `waiting_healthy`
the member is **not** told to drain, which would take down the process the restart started. The
phases are the ones a turn at a proxy has, without `retiring` and `undraining`, and no field was
added to the record. A `roll.json` in `retiring` or `undraining`, begun with a proxy, fails the
member with `swap_abandoned`: abort that roll rather than change `--proxy-admin` in the middle of
it.

## The agent mode

`kbf-updater agent` is the other mode of this program, and the one the maintainers' design
([../design/updates.md](../design/updates.md)) ends on: an unprivileged updater on every host that
dials the coordinator and owns that host's update. The roll mode above is unchanged and keeps
working (until the roll and its ssh path are deleted, in the last step of that design).

The program is a release asset for each platform that runs it: `kbf-updater-<commit>-linux-x86_64.tar.gz`,
`kbf-updater-<commit>-linux-arm64.tar.gz` and `kbf-updater-<commit>-darwin-arm64.tar.gz`, each with one
member, `kbf-updater`, attested like every tarball ([../artifacts.md](../artifacts.md)). The macOS one
is signed ad hoc with the hardened runtime, as the other macOS binaries are; a Mac runs the agent as
`_kbf-updater` from the property list in `packaging/host-agent/macos`.

```text
kbf-updater agent --coordinator <dns name>:<port> --channel NAME
                  (--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
                  [--frontend <dns name>:<port>]
                  [--operator-api http://IP:PORT --token-file FILE
                  [--operator-proxy-auth-file FILE]] [--helper-socket PATH]
                  [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
                  [--health-poll-secs N] [--retry-secs N]
```

- `--coordinator`: a DNS name and a port. The coordinator's certificate must chain to the farm CA
  for that name and have the role `coordinator`; that is checked after the handshake and before
  anything is sent.
- `--state-dir`: the directory `kbf-updater enroll` filled ([below](#enrolling-a-host)): the key,
  the certificate and the CA are its files. The agent renews the certificate at half its life
  and writes the new one there; `--frontend` is where. The certificate is the farm CA's `updater`
  certificate: its one DNS name is this node's id and its `urn:kbf:host-role:` is what the host
  runs (`storage`, `daemon` or `mac-daemon`; a `frontend` is refused, the roll updates frontends).
  The agent reads both from it. The files are read for every heartbeat and every call to the
  frontend, so a renewed pair is used from the next one, if it names the same node and role as the
  pair the agent started with (a pair for another node is refused until the agent is restarted).
  The key file must be a regular file of the agent's user with mode 0600 or 0400, as the token
  file must.
- `--client-cert`, `--client-key`, `--ca`: instead of `--state-dir`, files an operator placed.
  Nothing renews them. Not both.
- `--frontend`: the frontend's public address, a DNS name and a port (the kbf-proxy). The node's
  drain, undrain and status are calls there, made with the updater's certificate and **no token**;
  see [the boundary](#the-boundary). It is required with `--state-dir`.
- `--channel`: the channel of the helper's allowlist. The version the coordinator names is looked
  up in it by the helper, never fetched by URL.
- `--helper-socket`: the socket of `kbf-host-install --serve` (default
  `/run/kbf-host-install/local.sock`; [host-install.md](host-install.md#the-local-socket)).
- `--operator-api`, `--token-file`, `--operator-proxy-auth-file`: optional, together: the full
  operator token over the frontend's loopback, an opt-in fallback used instead of the
  certificate; see the boundary below.
- Times, in seconds, 1 to 86400: `--heartbeat-secs` 15, `--drain-deadline-secs` 600,
  `--health-timeout-secs` 300, `--health-poll-secs` 5, `--retry-secs` 900.

### The heartbeat

A POST of `{"platform", "version", "health", "last_update", "updating"}` to `/v1/heartbeat`, at
start and every `--heartbeat-secs`, on a thread of its own, so that it goes on through a whole A/B
(a host that is silent for 45 seconds counts as down). `version` is the version text the host's
helper knows for the release that runs; for a release placed by hand it is
`0.1.0+g<12 hex digits>`, which names the commit and nothing else, and an update to the same commit
teaches the helper the text. `health` is the node's health check against the running version.
`updating` is the target while an A/B runs; `last_update` is how the last one ended. **The node id
and the role are in no field**: the coordinator reads them from the certificate.

### One A/B

On a reply of `update to X` (and only X is used from it), in this order, each step only if the one
before it worked:

1. **Fetch and verify** through the helper (`install`): the release is fetched on the host and
   checked there. An unattested build, one of another commit, or one the channel does not have is
   refused here, and the node has not been touched.
2. **Drain** the node at the frontend (`:drain` of the operator API, as the roll mode does for a
   daemon host, with the updater's certificate through the proxy; a storage host is never drained) and wait for its leases, for at most
   `--drain-deadline-secs`.
3. **Switch** (`switch`): the helper puts X on trial and restarts the units.
4. **Health**: the node's health check (`node_health`: a storage member is `active` on the build, a daemon is connected on it) of X, held drained, for
   at most `--health-timeout-secs`, and never past the helper's deadline less ten seconds.
5. **Confirm** X if it is healthy. If it is not, **revert**: the helper does that by itself at its
   deadline whatever this process is doing; the agent asks for it at once.
6. **Undrain** only if the node is healthy: on X after a confirm, or on the release that ran
   after a revert. A node that is not healthy stays drained.

The result goes in the next heartbeat. A restart of the agent in the middle finds the helper's
pending marker (`query`) and finishes the trial from step 4; nothing is fetched, drained or
switched twice. A target that failed is not tried again before `--retry-secs`, however often
the coordinator names it.

### Enrolling a host

A host gets its updater certificate from the farm CA with a one-time token, with `kbf-updater enroll`:

```text
kbf-updater enroll --frontend <dns name>:<port> --node-id ID --token-file FILE --state-dir DIR
```

1. On the frontend the operator makes the token (`POST /v1/enroll/tokens`, [../api.md](../api.md)):
   `{"roles": ["updater"], "node_id": "host-1", "host_role": "daemon"}`, one use. The token fixes
   the node id and the host role; a token for a node's roles (`storage`, `daemon`) is refused for
   an updater, and an updater token is refused for any other role, for another node id, a second
   time and after it expired.
2. On the host, as the agent's account, the token goes in a file of that account's with mode 0600
   and the state directory is that account's with mode 0700 (the command refuses otherwise,
   before it reads the token). The command makes the key on the host (mode 0600; only a request
   signed by it is sent), checks the frontend against the CA pin the token carries and the answer
   (role, node id, key, chain) before it writes anything, installs `pki/updater.key`,
   `pki/updater.pem` and `pki/ca.pem`, and removes the token file. Run again after a success it
   finds the host enrolled and changes nothing; after a failure it leaves the token file for a retry.
3. The agent started with `--state-dir DIR` renews the certificate at half its life (spread by up
   to 10%), by a request signed by the key the registry holds, retrying every five minutes
   doubling to an hour, and logs an error when less than a day is left. A host whose certificate
   lapsed during an outage recovers by itself, for as long as the host is not replaced.

Revoking the host by its id (`POST /v1/enroll/nodes/{id}/revoke`) revokes its updater row too, so the
renewal is refused and the certificate runs out at its end. The proxy does not read the deny list,
so a certificate already issued keeps its three own-node calls (drain, uncordon, status) until it
expires: seven days by default, thirty at most. A host re-enrolled with a `replace` token stops
the old key from renewing in the same way and does not cut off a certificate already issued.
The `deadline_secs` of a self-drain is whatever the host sends; it only moves that host's own
drain deadline.

The CA's key is the frontend's alone; the coordinator holds none. The units in
`packaging/host-agent` use this layout ([its README](../../packaging/host-agent/README.md)).

### The boundary

The design says the full operator token must not be on hosts, and the agent does not need it. The
drain of a node (`NodeDrain`: cordon, drain, uncordon) and the node's status for its health are
calls to the operator API, which is on the frontend's loopback. The agent makes them through the
frontend's public proxy with its updater certificate: **the proxy admits, for that certificate,
`POST /v1/nodes/{id}:drain`, `POST /v1/nodes/{id}:uncordon` and `GET /v1/nodes/{id}` for the
certificate's own node id, and nothing else** (another node's id, the cordon, a listing, the update
API and everything else is `403`; so are the worker and build-cache paths whatever `buck2_auth`
says), and puts the operator token in itself. The shipped units give the agent no token. The proxy's certificate for the name in `--frontend` must be the farm CA's `frontend` certificate (the agent checks the chain to the farm CA, the name and the role before it sends a byte), not a public one.

What remains of the token: `--operator-api` and `--token-file` (and the proxy secret file) are an
opt-in fallback for a host whose operator accepts the token there (anything running as the agent's
account can then read it); given, they are used instead of the certificate. A **storage** host
needs them: its health is read from `GET /v1/enroll/nodes` and `GET /v1/members`, which a
certificate cannot reach, so a storage host's agent without them heartbeats and refuses every update
order before it fetches anything, and the roll updates the host. Without `--frontend` and without
the token an agent does the same for any host. The mechanism is behind two small ports (`Drain`,
`HealthCheck`), so that a storage-health route a certificate may call replaces the refusal and not
the agent.

## Turn on the deploy step

The last job of `release.yml`, `deploy`, tells the update coordinator "desired = this release" once
`publish` has made the release, and waits until the farm converges. It is the end-to-end check of
a release on a real farm. It does nothing until an operator has done the three things below, once;
until then the job prints `::notice::the deploy step is off` and passes, so a fork or a repository
that runs no farm is unaffected. Nothing in this repository does any of them.

1. **Deploy the coordinator** (`kbf-coordinator`, [the design](../design/updates.md)) with a
   certificate the farm CA issued with the role `coordinator`, and an allow-list for CI's OIDC
   token in the file `--oidc-config` names. The job's token is the run's, so the allow-list must
   say exactly this, and nothing wider:

   ```toml
   [remote_operator]
   issuer = "https://token.actions.githubusercontent.com"
   audience = "https://<the coordinator's control address, as written in the variable below>"
   repository_id = "<the numeric id of komira-ai/kbf>"
   repository = "komira-ai/kbf"
   job_workflow_ref = ["komira-ai/kbf/.github/workflows/release.yml@refs/heads/main"]
   refs = ["refs/heads/main"]
   environments = ["release"]
   event_names = ["workflow_run"]
   ```

   `event_names` must name `workflow_run`: the release workflow is started by the completion of
   the pipeline, and its tokens say so (the default list is `push` and `workflow_dispatch`).
   The `sha` of the token is the head of `main`, not the commit released. A token is good for one
   call. The control listener (`--control-listen`) must be reachable from GitHub-hosted runners;
   the heartbeat listener need not be.
2. **Enroll the hosts**, so that their updaters heartbeat to the coordinator
   ([The agent mode](#the-agent-mode)). A host that does not heartbeat is `down`, and the
   coordinator does not wait for it: it says `converged` when every host that does heartbeat runs
   the release, none is updating or failed, and at least the minimums (below) are up. The step
   does not take that alone: it remembers how many hosts of each role were live when it started
   (see below) and fails if fewer run the release at the end, so a host that the release made
   stop heartbeating fails the deploy. A host that was already down when the step started is not
   expected; raise the minimums to the number of hosts the farm must keep, for the roles whose
   count matters beyond that.
3. **Set the repository variables** (Settings, Secrets and variables, Actions, Variables):

   | variable | value |
   |---|---|
   | `KBF_COORDINATOR_URL` | `https://<name>:<port>` of the control listener, no path. Unset or empty: the step is off |
   | `KBF_COORDINATOR_CA` | the farm CA's certificate (PEM, public). The coordinator's certificate is the farm CA's, which no system store holds; empty trusts the system store instead |
   | `KBF_MIN_UP_STORAGE`, `KBF_MIN_UP_FRONTEND` | optional: the storage hosts and frontends that must stay up while others update (default 1 each). The operator picks the storage number so that every blob stays readable |

### What the step does

`cargo xtask deploy-farm` (one `run:` line):

1. reads `GET /v1/convergence` once, before anything is changed, and keeps the hosts live then
   (at the version, updating or failed, so not `down`) per role as the baseline, and prints it
   (`baseline: hosts live before the deploy: storage 3, frontend 2`). A 503 on this read is tried
   again like the POST. A coordinator that has just restarted (the table says so) may not have
   heard from every host yet; a host it hears from later counts as one that joined;
2. posts `{"version": <the release's version>, "min_up": {...}}` to `/v1/desired` (the per-role
   limits are the coordinator's defaults), with an OIDC token for the audience that is the URL as
   written in the variable (the audience keeps a trailing `/` the variable has, which the URL
   the calls go to does not, so write the variable and the allow-list's `audience` alike). A 503
   on the POST is tried again, up to four times;
3. reads `GET /v1/convergence` every 30 seconds, with a fresh token each time, and prints a
   table of the counts per role (`at_desired`, `updating`, `failed`, `down`);
4. ends when the coordinator says `converged` and every role has at least as many hosts at the
   version as it had live at the baseline (exit 0; a host that joined during the deploy only
   adds); when it says `converged` and a role has fewer, which fails with the role, the count
   expected, the count reached and how many went down; `blocked` (a live host failed its update to
   this version and is still behind it); when the counts have not changed for 20 minutes
   (`--stall-min`; a host whose update takes longer than that leaves the counts as they are, so
   raise the flag past the longest update); when the coordinator's desired version is no longer
   this release's (another writer replaced it); when 60 minutes are up (`--deadline-min`); or when the coordinator refuses
   the token (401 or 403, at once: the allow-list does not match this job). Any of those but
   convergence fails the job; the release stays published whatever it says. Another failed read
   is reported and tried again, and counts as no change.

`GET /v1/convergence` answers `{"desired", "quiet", "state", "roles": [{"role", "at_desired",
"updating", "failed", "down"}]}`, where `state` is `converged`, `blocked` or `in_progress`. It
takes CI's OIDC token and refuses the operator token (the operator's read is `GET /v1/status`).
It is computed from the heartbeats and the desired state, and stores nothing. `converged` needs
the coordinator to be past its 30 second quiet start, at least one live host, every live host at
the version and none updating or failed, and `min_up` hosts of the storage and frontend roles up,
so a coordinator that has just restarted does not report a farm it has not heard from as
converged.

The `deploy` job runs in the `release` environment, like `publish`: if the environment has
required reviewers, a release is approved a second time before the job starts.

One release at a time holds across the deploy: the workflow's group does not cancel, so a second
release waits for the first release's deploy (at most 90 minutes) to end.
