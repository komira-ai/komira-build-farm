# Rolling a commit out to the fleet: `kbf-deploy rollout`

`kbf-deploy rollout` puts one commit on every host of a fleet, one host at a time,
and stops at the first that does not go right. It is the orchestrator: it decides the
order and the gates and undoes a failure, and it reaches a host only through the
`Transport` trait (`crates/kbf-deploy/src/rollout/transport.rs`), whose docs are the
contract the host verbs and the transport are built against.

```text
kbf-deploy rollout --inventory FILE --commit SHA --run-id ID
                   [--idle-timeout SECONDS] [--pause-file FILE] [--apply]
```

Without `--apply` it prints the plan as `DRY-RUN:` lines and touches nothing: no lock,
no host, no connection. With `--apply` it prints `+ <step>` as each step starts, the
same lines the plan lists. A binary built without a transport refuses `--apply`.

## The inventory

One host per line, `NAME ROLE`; blank lines and `#` lines are skipped. Roles:
`storage-and-daemon`, `frontend` (exactly one), `daemon` (Linux) and `daemon-mac`.

The order comes from the roles, not from the file: every `storage-and-daemon` host,
then the frontend, then every `daemon` host, then every `daemon-mac` host. The file
orders hosts of one role.

## What a rollout does

1. Takes the fleet lock as `--run-id`. A second rollout finds it held and is refused
   (exit 2) without touching anything.
2. Backs up the frontend's state file.
3. For each host, in order (nothing after a failed host is touched):
   - **gate**: no blob in the cluster is under-replicated;
   - **stage**: the files of the commit on the host, verified there;
   - **drain**, then wait until nothing is in flight: leases on a daemon (15 minutes on
     Linux, 60 on a Mac), open Execute streams on the frontend (`--idle-timeout`,
     default 6 hours). A lease or a stream is never killed. If the wait runs out the host
     is undrained and as it was, and the run ends *pending* (exit 3);
   - **swap** to the commit, then wait until the host is healthy at it (unit active,
     ready, registered `active` at the commit; 120 seconds) and, on a
     `storage-and-daemon` host, until no blob is under-replicated (5 minutes);
   - **confirm**, which cancels the host's own revert timer, and **undrain**.
4. Releases the lock.

A host that fails after its swap, or whose swap call returned an error (the reply may
have been lost after the work was done), is rolled back with the same verbs and
undrained; the run ends red (exit 1) with where the host was left. A host that cannot be
rolled back, or undrained, stays drained and the message says so: it needs a person.

At most one host is drained at a time, so never two storage nodes at once.

## Pausing

While the `--pause-file` exists the rollout takes no further host. It is read before the
run and before each host, never in the middle of one, so a host in flight finishes (or
is rolled back) and none is left drained. A paused run exits 3.

## Exit codes

0 done (or planned), 1 a host failed, 2 refused before any step, 3 stopped with
nothing wrong (paused, or a host still had work in flight).

## Tests

The rollout's tests run it against a simulated fleet that implements `Transport`
(`crates/kbf-deploy/src/rollout/fleet_tests.rs`): it logs every call, keeps its own
clock (moved only by `Transport::sleep`, so no test waits or asserts a duration of the
real clock) and fails on request. The planted faults are in
`crates/kbf-deploy/src/rollout/engine_tests.rs`.
