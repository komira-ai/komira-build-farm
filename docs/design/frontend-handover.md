# Updating the frontend without failing a build

`kbf frontend handover` starts a new frontend beside the serving one, the new one takes the
listeners, the state store and the daemons over, and the old one finishes the builds it
holds and exits. This page says what each party sees. The protocol between the two
processes is `kbf_server::handover` (`architecture-v2.md`, sections 11 and 12); the unit
and the command are here.

## The unit: `kbf-frontend@.service`

One template, two instances, `kbf-frontend@a` and `kbf-frontend@b`, with the same command
line. Only one serves at a time. `linux install-units --mode frontend` writes the template,
enables it (`DefaultInstance=a`: only `@a` starts at boot) and starts `@a` if neither
instance is up. A serving instance is never restarted by an install: the plan says to run
the handover instead. What the unit has, and why (check-units rule `handover`; rule `stop`
still asks for `TimeoutStopSec=infinity` and `KillSignal=SIGTERM`):

- `Type=notify`: kbf sends `READY=1` once it serves and its probes pass, so
  `systemctl start` of the idle instance returns when the new frontend can take builds, and
  fails when it could not.
- `TimeoutStartSec=120`, finite: a new instance that never says ready fails the start.
- `--handover-socket=/run/kbf-frontend/control.sock`, in `RuntimeDirectory=kbf-frontend`
  with `RuntimeDirectoryPreserve=yes`: the socket's directory outlives the instance that
  stops. Both instances run as the same user (the socket checks the peer's uid).
- No `Conflicts=` between the instances: starting one must never stop the other, which is
  the one draining.

## The command

```
kbf frontend handover [--systemctl PATH] [--wait [--poll-ms N]]
```

1. It reads each instance's `ActiveState` (`systemctl show -p ActiveState --value`). Exactly
   one `active`: that one serves. None: nothing serves, and it starts `@a` (a first start,
   or a start after a crash). Both `active`: an earlier handover's old instance is still
   draining its builds, and the command refuses (exit 1) and starts nothing; run it again
   when the old one has exited. An instance `activating` or `deactivating` is refused the
   same way.
2. It runs `systemctl start` on the idle instance and waits for it. The new binary must be
   in place first, put there by rename (a running frontend keeps its old file); the updater
   does not restart what it replaces.
3. On success it prints that the new instance serves and the old one drains and exits by
   itself, and exits 0. With `--wait` it then looks at the old instance every `--poll-ms`
   (1000) until it is no longer active: exit 0 for a clean exit, 1 if it ended in failure.
4. If the start fails it exits 1 and says whether the old instance is still active. A
   successor that fails before it said `ready` changed nothing: the old frontend keeps
   serving. The only other end is the one in "When it goes wrong" below.

The only verbs it runs are `show` and `start`: it never stops, restarts or kills.

## What each party sees

**Buck2 builds in flight (open `Execute` and `WaitExecution` streams).** They stay open and
are not told to go away. The old frontend answers each from the `done/` row the new one
writes when the action finishes, so Buck2 receives the result of its action as it would
have from one process. While the action is leased the stream shows it executing; while it
is queued it shows what it showed. If the store has no trace of an operation (neither a
`done/`, an `op/` nor a queue row) for three looks in a row, which is the new frontend
having lost it, the stream ends UNAVAILABLE and `kbf_frontend_relay_lost_total` counts it:
a stream is never left waiting for ever. Buck2 does not recover a stream that breaks, so
that count is the number of builds that failed because of a handover; it is zero unless the
new frontend died between two writes.

**New Buck2 calls.** Until the old frontend stands down they are served by it. From then
on it sends GOAWAY on its REAPI connections, so a client opens a new connection to the
same address. The listening socket is the same one, so the connection waits in its backlog
until the new frontend serves it; a call that raced the GOAWAY on an old connection gets
UNAVAILABLE at open, which Buck2 asks again. Nothing is refused for lack of a listener.

**Daemons.** The old frontend ends their streams; each dials the same worker address again
and reaches the new frontend. The epoch is kept, so a daemon keeps the runs of its leases
and sends again the results the old frontend did not acknowledge. No scheduling happens
between the old frontend's stand-down and the new one serving (normally well under a
second; the wait for calls between their request and their queueing is bounded by
`--handover-quiesce-secs`, 10 s by default).

**The operator API (`kbf cluster`).** Its listener stops on the old frontend and the same
socket serves from the new one; a call in the gap waits in the backlog. A cordon or a
drain an operator set is a row in the state store, so the new frontend still holds it (a
drain's deadline is wall-clock time).

**`/readyz` and metrics.** The instances share the ports, so `/readyz` answers from the
new frontend once it serves. The old one's own state is only systemd's: `active` until it
has answered its last stream, then it exits.

## What a handover carries and what it does not

**Carried, as rows of the state store.** The operations and their queue order (`op/`,
`queue/`), the leases (`lease/`), the loss count of an operation (`loss/`), the result of a
finished one (`done/`), the daemons' sessions, the cordons and drains, and the epoch. The
old frontend writes the leases it has given up before it gives up the writer role: it
retries, for at most `--handover-quiesce-secs`, the requeues the store refused, and stops
at once if the store says a newer writer exists. If the store still refuses at the
deadline, the handover completes and the old frontend logs an ERROR naming the operations
it could not write; the new frontend then recovers each of them with the lease the store
still holds, and the daemon's heartbeat or the lease grace settles it.

Rows for the state that is in memory today and in the stack of changes #897, #911, #915 and
#916 (`fate/` for the memory ladder and the fault budgets of an operation, `floor/` for the
memory floors, `subject/` for the usage line of an operation) are carried only once those
changes are merged; until then a successor starts those from nothing.

**Not carried.**

- An invocation record is split between the two processes: an invocation in flight, or a
  recent one, looked up through `/v1` or the UI on the new frontend is not found. Build
  results are not affected, because callers are answered from `done/`.
- Counters restart at 0 in the new process (the `pressure` counter included), so a sum
  across processes scraped one by one sees a drop. Use `rate()` or `increase()`.
- `action shape:` is logged again, once per key, by the new process.
- The unservable timer restarts: a refusal can be delayed by up to `unservable_wait`.
- `kbf_front_open_executions` stays above zero in the old process for as long as it
  answers callers from the store.
- A kill whose store write was not acknowledged before a crash costs at most one extra run
  or one extra budget step.
- The rule "seen before: the last measured peak plus a buffer" for the memory a key is
  booked is not implemented; the floors are only the highest rung an operation was raised
  to.

**Known gap.** Two other refused writes are not retried, before or at a handover. When
the store refuses the rows of a committed record (an `Effect::Commit`), the farm logs it
and drops the effects that depended on it; when it refuses the `done/` row of a finished
operation, the farm logs it and answers its callers from memory. Neither is written again
by the old frontend, so the new one recovers such an operation from the rows it finds: the
lease it was granted, or no result. An issue is to be filed for it.

## When it goes wrong

| What happens | What clients see | What the operator does |
|---|---|---|
| The new instance fails flags, certificates or probes, or is stopped, before `ready` | Nothing: the old frontend serves | Read `journalctl -u kbf-frontend@<new>`, fix, run the command again |
| The new instance dies after the old one released the store | Callers of the old one still get their results from the rows; the new instance is started again by systemd (`Restart=on-failure`) as a cold start, which bumps the epoch: the daemons run their leases again | Check `journalctl`; builds are not failed, actions rerun |
| The old frontend dies mid-handover | Its open streams end, as for any crash | The new one proceeds (the kernel released the file lock) |
| A different `--listen`, `--worker-listen`, `--api-listen` or state file in the new unit | The handover is refused with the reason and the old frontend serves | A change of address is a cold restart: `systemctl stop` the serving instance (it drains) and start the other |
| Both instances active | The command refuses | Wait for the old one to exit |

A handover moves a process, not a machine: the two instances run on one host, on one
state file. `--handover-socket` needs `--state-store`, and a start with the flag but
without a state store is refused.
