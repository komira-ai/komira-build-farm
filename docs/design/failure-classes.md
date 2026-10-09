# Failure classes: farm, request, action

When an action fails, the person reading the build log needs one answer first: is this
mine to fix, or the farm's? This document defines the classes of failure, what the
farm does today, and what is **proposed** so that a farm problem is never reported as
the action's own failure. Lease handling is in [daemon.md](daemon.md), outcomes and
retries in [scheduler.md](scheduler.md#outcomes), node reports in
[capabilities.md](capabilities.md).

Line numbers are on `main` at the merge of #177. "Proposed" means no code yet.

## 1. The classes

| Class | Meaning | Whose to fix | Example |
|---|---|---|---|
| **Farm** | The farm could not run the action, or ran it on a node unfit to run it. | the operator | Xcode licence not accepted; scratch disk full; lost contact; a blob the CAS lost |
| **Request** | The action cannot run as written, on any node. | the client | a Command with no arguments; an output over the size limit; a blob never uploaded |
| **Action** | The action ran on a fit node and exited non-zero, or ran past its timeout. | the action's author | a compile error; a failed assertion; a crashed test harness |
| **Ambiguous** | It ran and failed, and the farm cannot yet tell Farm from Action. | decided by a second run | a SIGKILL kbf did not send; a timeout on a node under pressure |

**Test failure vs test error** is a split inside Action, and it belongs to the client.
REAPI has one field for it, `ActionResult.exit_code`; the difference between "the
assertion failed" and "the fixture crashed" lives in the test runner's exit codes and
its JUnit XML. The farm passes both through untouched. What the farm owns is the line
between Farm and everything else.

**The invariant.** *A farm-side problem never reaches the client as the action's exit
code.* Equivalently: an `ExecuteResponse` with status OK means a fit node ran the
action to the end. Two corollaries:

- A Request error never reaches the client as `INTERNAL` (the client would wait for
  an operator who has nothing to fix).
- Every non-OK answer says which class, which node and why, in words a person can act
  on.

## 2. What the code does today

The daemon sends one `Result` per lease: status OK with an `ActionResult` whatever the
exit code, or a non-OK status (`kbf-proto/proto/kbf/worker/v1/worker.proto:240-251`).

- **Daemon** (`kbf-daemon/src/lease.rs:157-182`): `Killed` is `ABORTED`, `Failed` is
  `INTERNAL`, `Invalid` is `INVALID_ARGUMENT`, `MissingBlob` is `FAILED_PRECONDITION`
  with a `MISSING` violation, `TimedOut` is `DEADLINE_EXCEEDED`, `OutOfMemory` is
  `RESOURCE_EXHAUSTED`. Before a run it may refuse `FAILED_PRECONDITION` (lease kind not
  served, `lease.rs:69-75`) or `UNAVAILABLE` (contact lost, `daemon.rs:467-475`). Its log
  says only `ok=false` (`lease.rs:119`).
- **Server** (`kbf-server/src/farm.rs:518-557`): OK with stored outputs is `Completed`;
  `DEADLINE_EXCEEDED` is `Timeout`; `INVALID_ARGUMENT` keeps its message; **every other
  code is `Failure::Infra` and the daemon's message is dropped**. The log has the code
  only (`farm.rs:553`).
- **Answer** (`farm.rs:843-870`): Infra becomes `INTERNAL "the farm could not run the
  action"`, the same fixed text for every cause, with no node name.
- **Retries**: none. A failed result finishes the operation
  (`kbf-sched/src/scheduler.rs:709-730`), though
  [scheduler.md](scheduler.md#outcomes) plans bounded retries (#22). Only a lease
  given up (silent, replaced, reconnected, not started:
  `kbf-sched/src/requeue.rs:28-41`) runs again, with no limit.
- **Cache**: written only for exit 0 and not `do_not_cache` (`farm.rs:849-850`).
- **Readiness**: the node report is detected once, at daemon start; re-detection is
  planned ([capabilities.md](capabilities.md), "Re-detection"). On macOS an Xcode is
  advertised if `xcodebuild -version` answers (`kbf-driver-native/src/xcode.rs:55-63`).
- **Alerts**: none. `kbf-alert` holds a one-line crate comment and no code.
- **Metrics**: no metrics endpoint exists.

The pilot showed the gap. A Mac advertised an Xcode whose licence was not accepted;
`xcodebuild -version` exits 0 without one. Every action routed there exited 69 ("You
have not agreed to the Xcode license") and reached buck2 as an ordinary failed action,
buck2 exit 3, a user error.

### Where a farm problem leaks out as an exit code today

1. A toolchain the node cannot use: licence, Metal or another component missing, an
   SDK missing, a broken `xcrun` cache.
2. A sandbox profile that denies what it should allow (#172's case: exits 1 and 74).
3. A signal kbf did not send: macOS memory pressure, a Linux OOM daemon, an operator's
   `kill`, a session teardown. The native driver records 128+N
   (`kbf-driver-native/src/runtime.rs:536-541`), the same bytes as a test that called
   `exit(137)`.
4. A kernel OOM kill of a child in a container whose parent exits with its own code.
   Only exit 137 with a counted `oom_kill` is caught
   (`kbf-driver-container/src/runtime.rs:300-311`).
5. Scratch disk full while the action runs.
6. A slow or paused node pushing an action past its timeout (arrives as
   `DEADLINE_EXCEEDED`, blamed on the action).
7. Environment drift: a missing tool on `PATH`, the wrong `DEVELOPER_DIR`, leftovers
   of another lease.

### And the other way round

- Program not found (`kbf-driver-native/src/runtime.rs:172`), an output over the size
  limit (`runtime.rs:636`), and a missing input blob all reach the client as the fixed
  `INTERNAL`. The `MISSING` detail is dropped at `farm.rs:553`, so Bazel is never told
  to re-upload.
- The named Xcode missing on the node is `RuntimeError::Failed`; the comment at
  `xcode.rs:190` says "the action is run elsewhere", but nothing reruns it.
- Native over-booking OOM is `RESOURCE_EXHAUSTED`; the comment at
  `kbf-daemon/src/runtime.rs:46-49` says "the farm's to retry", but nothing does.

### What the open pull requests change

- **#173** runs `xcodebuild -license check` and `xcrun --find clang` at daemon start and
  leaves out an Xcode that fails. It checks once, logs, and raises no alert: actions
  then wait 300 s for a refusal, or capacity quietly shrinks. It touches `discover`,
  as #172 does; expect a conflict.
- **#172** fixes the sandbox denials for temp items and the `xcrun` cache. #161
  (orphaned runs) and #174 (retention) change nothing a client sees.
- Merged: **#176** ends open Execute streams `UNAVAILABLE` on shutdown; **#177** names
  the worker on OK results only, and logs each requeue with its reason.

None adds retries, keeps the daemon's reason, alerts, or classifies after a run.

## 3. What the clients do with each answer

Read from the source of buck2 (`main`), Bazel (`master`) and the REAPI proto on the
day this was written. The parts that decide the design:

| Answer | buck2 | Bazel |
|---|---|---|
| OK, exit non-zero | user error, exit 3; never retried | `NON_ZERO_EXIT`, user error; test `FAILED`; never retried |
| `INTERNAL` in the response | INFRA, exit 2; message shown; never retried | retried (`--remote_retries`, default 5), then exit 34 `REMOTE_ERROR` |
| `UNAVAILABLE` in the response | INFRA, exit 2; never retried | retried, then **catastrophic**: stops the build even with `--keep_going` |
| `FAILED_PRECONDITION` | USER, exit 3 | not retried, unless every violation is `MISSING`: then re-upload and re-Execute |
| `DEADLINE_EXCEEDED` in the response | ENVIRONMENT, exit 2; result dropped | `TIMEOUT`; partial stdout and outputs shown |
| `RESOURCE_EXHAUSTED` | USER, exit 2 | retried |
| `INVALID_ARGUMENT` | INFRA, exit 2 | not retried, exit 34 |
| `ExecuteResponse.message` | printed as "Info:" after a failed action's output | printed as "Remote server execution message:" on failure |
| `server_logs`, `worker`, `auxiliary_metadata` | never shown | `server_logs` path only with `--verbose_failures` |

Three facts follow:

1. **buck2 never retries a failure inside the Execute stream**, only opening it. A farm
   fault that should run elsewhere must be rerun by the server; a status that hopes for
   a client retry does nothing for buck2 users.
2. **`status.message` is the only text buck2 shows on a non-OK answer.** The class, the
   node and the operator's fix go there.
3. **`FAILED_PRECONDITION` is the wrong code for a farm fault**: buck2 calls it a user
   error.

## 4. Detection (proposed)

Three layers: before placement, after a run, and the errors the farm already sees.

### 4.1 Readiness probes, before placement

A **probe** proves the node can use a capability it advertises. Each capability that
needs more than "the file exists" names its probe:

| Capability | Probe | Fix text on failure |
|---|---|---|
| an Xcode build | `xcodebuild -license check`, `xcrun --find clang` (as #173) | `sudo xcodebuild -license accept` (with that Xcode's `DEVELOPER_DIR`) |
| Metal in that Xcode | `xcrun --find metal` | `xcodebuild -downloadComponent MetalToolchain` (Xcode 26 and later) |
| each SDK the node reports | `xcrun --sdk <sdk> --show-sdk-path` | install the platform in Xcode |
| the sandbox profile | a canary action run under the profile: writes its temp dir, runs `xcrun --find clang` | names the denied operation |
| scratch space | free bytes and inodes above a floor | names the directory and the floor |
| container images | the image check the driver already does | names the image |

Probes run at start, every 10 minutes (the period capabilities.md already plans for
re-detection), at once after any classified farm fault on that node, and on an
operator's request. A failing probe:

- withdraws **only that capability** from the node's report (the node keeps serving
  everything else), and sends the changed report mid-session, which needs the planned
  report-change handling (capabilities.md, "Report changes noticed by hash");
- raises an alert with the fix text (section 8);
- is retried on its period; when it passes, the capability returns and the alert
  resolves. Recovery needs no one to restart anything.

The scheduler keeps a capability that was withdrawn by a probe apart from one no node
ever had. A request that needs it waits, and if the wait bound passes it is answered
as a Farm fault naming the nodes and the fix, not as `FAILED_PRECONDITION`
(`farm.rs:825` today).

### 4.2 Known signatures, after a run

A **signature** is a rule: platform, exit code, a fixed substring of stderr, and the
probe that proves it. Signatures are data in the repository, reviewed like code. The
first set:

| Platform | Exit | Stderr contains | Proving probe |
|---|---|---|---|
| macOS | 69 | `agreed to the Xcode license` | licence check |
| macOS | any | `xcrun: error: unable to find utility` | `xcrun --find <that utility>` |
| macOS | any | `cannot execute tool 'metal'` | Metal probe |
| any | any | `No space left on device` | scratch space |

When a run exits non-zero and matches a signature, the daemon runs the proving probe
before it reports. **The signature is a hint; the probe is the proof.** Probe fails:
the result is a Farm fault, the capability is withdrawn, and the alert fires. Probe
passes: the result is an ordinary Action result, and a counter records the unproven
match. A test that prints the licence message on purpose, or a tool that wraps
`xcodebuild` and exits 69 for its own reason, is never rerouted.

### 4.3 Errors the farm sees itself

These need no guessing, only keeping the reason the daemon already writes:

- **Driver errors** (`RuntimeError::Failed`): setup, cgroup, podman, CAS I/O, clean.
  Farm. A clean that fails after a good run (`kbf-driver-native/src/runtime.rs:352`)
  keeps the good result and withdraws the node's scratch capability instead of
  discarding the work.
- **Who sent the signal.** The daemon already knows each kill it makes (timeout,
  cancel, fence, over-booking). Any other `SIGKILL` or `SIGTERM` is Ambiguous. A
  signal the program raises on itself (`SIGSEGV`, `SIGABRT`, `SIGBUS`, `SIGILL`) is
  Action.
- **Kernel OOM.** Read the lease cgroup's `oom_kill` count for every exit, not only
  137 (section 6.1).
- **Control faults**: fence (`ABORTED`), contact lost (`UNAVAILABLE`), lease kind not
  served, the named Xcode absent, outputs not stored (`farm.rs:539-542`). Farm.
- **Request errors**: no arguments, image by tag, output path in the input root
  (today's `Invalid`); an output or stdout over the size limit; a blob the client never
  uploaded.
- **Missing blob at the worker.** The front already checked the inputs, so a blob the
  worker cannot find is either lost by the CAS or a fetch error. The server checks the
  CAS: absent, it answers `FAILED_PRECONDITION` with `MISSING` (Bazel re-uploads) and
  counts a lost blob as a farm fault for the operator; present, it is a fetch error on
  that node, Farm, rerun elsewhere.

## 5. What the client receives (proposed)

| Class | Status | Exit code | Text | Cached |
|---|---|---|---|---|
| Action | OK | the action's | `ExecuteResponse.message` names the node and any farm reruns before it | exit 0 only, as today |
| Action, timeout | `DEADLINE_EXCEEDED` with the partial result (#45) | none | names the node and the timeout | no |
| Farm, rerun succeeded | OK | 0 or the action's | `message`: "ran on B after a farm fault on A: <reason>" | as Action |
| Farm, attempts used up | `INTERNAL` | none | `status.message`: "kbf farm fault on <node>: <reason>. Operator fix: <fix>. Attempts: n." | no |
| Request | `FAILED_PRECONDITION`, or `INVALID_ARGUMENT` for a malformed action (section 6.7) | none | the limit or field at fault | no |
| Missing blob | `FAILED_PRECONDITION` + `MISSING` | none | the blobs | no |
| Server shutting down | `UNAVAILABLE` (#176) | none | as today | no |

Every non-OK status also carries a `google.rpc.ErrorInfo` with domain `kbf` (the front
already uses it for `NO_WORKER_CAN_RUN`, `kbf-front/src/execution.rs:92-95`), reason
`FARM_FAULT`, `REQUEST_ERROR` or `ACTION_TIMEOUT`, and metadata `node`, `signature`,
`probe`, `attempts`. No client reads it today; it is for our UI, our CLI and any client
that learns to.

## 6. Open decisions, with options

### 6.1 Out-of-memory attribution

- **A. Booking decides.** Used more than the lease booked: the action's (Request if the
  client set the booking, a farm under-estimate if the estimator did). Killed below its
  booking: Farm (the node overcommitted).
- **B. Always Farm, rerun with a larger booking** (double, up to the largest node);
  only past the largest node is it the action's.
- **C. Always the action's** (today's native behaviour, without the retry).

A blames correctly but needs the cgroup count on every exit and, on macOS, a way to
see a memory-pressure kill (UNVERIFIED that `kern.memorystatus` or the unified log
gives it cheaply). B is simple and self-healing but hides a real leak behind reruns. C
is wrong for node pressure. **Lean: A with B's retry.** Over-booking is rerun once with
twice the booking; if it fails again on the largest node that fits, answer
`FAILED_PRECONDITION` "used X, limit Y" (not `RESOURCE_EXHAUSTED`: Bazel would retry it
five times at the same size). Killed below its booking: Farm. A SIGKILL kbf did not
send and cannot attribute: Ambiguous.

### 6.2 Timeouts on slow or paused nodes

- **A. Always the action's** (today).
- **B. Measure the node.** The daemon records, per lease, time the machine was
  suspended (monotonic clock against boot clock on Linux, continuous against absolute
  time on macOS), time the daemon or child was stopped, and node pressure (Linux PSI
  `full` on CPU, memory, IO). Above a threshold the timeout is Farm and reruns
  elsewhere with the same timeout.
- **C. Rerun every timeout once** elsewhere.

C doubles the cost of every real hang. A blames the action for a sleeping laptop-class
node. **Lean: B**, with thresholds set from pilot data; and #45 so an Action timeout
shows its partial output (Bazel shows it; buck2 drops the result on any non-OK status
and cannot).

### 6.3 Flaky detection

- **A. None in the farm**: clients have `--flaky_test_attempts` and their own reruns.
- **B. Record, do not act.** Keep outcome history per action digest and per node; a
  digest that fails on one node and passes on another is flagged; a node whose
  failures pass elsewhere more often than its peers' raises a node-suspicion alert.
- **C. Rerun every Action failure** once to detect flakes.

C doubles the cost of every real failure and hides flakes from their authors. **Lean:
B.** The farm reruns only Farm and Ambiguous results; the node-suspicion alert is how
an undetected farm fault (one with no signature yet) gets found.

### 6.4 Known signatures: allowlist or heuristics

- **A. Allowlist per platform**, each entry with its proving probe and fix text.
- **B. Generic heuristics**: exit 69, 126, 127, "permission denied", "license".
- **C. Both**: allowlist decides Farm; heuristics only count, to suggest new entries.

B misclassifies user failures (stderr is the action's to write) and so reruns them.
**Lean: C**, with every Farm verdict confirmed by a probe (section 4.2).

### 6.5 Who retries a farm fault

- **A. The server**, on a different node: the scheduler excludes nodes that faulted on
  this operation, at most 2 extra attempts, then `INTERNAL`.
- **B. The client**: answer `UNAVAILABLE` and let it retry.

B does nothing for buck2 (section 3), and Bazel treats `UNAVAILABLE` as catastrophic.
**Lean: A.** Farm-fault attempts do not count against the action; the operation keeps
its queue time. Bazel will retry an `INTERNAL` five more times on top, so after the
attempts are used up the server remembers the action digest's Farm verdict for a short
time (60 s) and answers a repeat at once, rather than running 15 more doomed attempts.
This needs a `Failure::Farm` outcome the scheduler requeues from (today `Failure::Infra`
finishes the operation, `kbf-types/src/work.rs:138-147`), the excluded nodes in the
operation's state, and the reason kept beside the outcome as `Detail::Invalid` keeps
its reason today.

### 6.6 How the class reaches buck2 and Bazel

- **A. Status code and `status.message` only** (section 5).
- **B. A also, plus a client-side shim**: a buck2 `error_handler` or a Bazel module
  that reads `ErrorInfo`.
- **C. Encode the class as an exit code** (for example a reserved exit for Farm).

C breaks the invariant. B is unproven: whether a buck2 `error_handler` category can
change the error tier is UNVERIFIED. **Lean: A now**; the `ErrorInfo` is there for B
when a client can use it.

### 6.7 Request errors: which code

`INVALID_ARGUMENT` is what REAPI names for a malformed action, but buck2 tags it INFRA.
`FAILED_PRECONDITION` gives buck2 USER and Bazel no retry. **Lean:** keep
`INVALID_ARGUMENT` for a malformed action (rare, and spec-named); use
`FAILED_PRECONDITION` for output over the size limit and over-booking. Program not
found: a path inside the input root is Action (exit 127 with kbf's message on stderr,
as a shell and podman would); an absolute path outside it, missing on this node while
peers of the same platform have it, is environment drift, so Farm.

## 7. Labelling: where the class is recorded

- **Client**: section 5's status, message and `ErrorInfo`; `ExecutedActionMetadata.worker`
  on every result, failed ones included (in the partial result where there is one).
- **Server log**: one line per attempt with operation, lease, node, class, reason,
  signature and fix. Today it has the code only (`farm.rs:553`).
- **Daemon log**: the reason and class beside `lease finished` (`lease.rs:119`).
- **`server_logs`**: an attempt history blob per operation (each attempt's node,
  class, reason, stderr tail). Bazel fetches it on failure; the UI reads it.
- **`/v1/nodes`**: per node, each capability's probe state (ready, withdrawn with
  reason and fix, since when) and the last farm faults.
- **Usage records** (planned in scheduler.md, "Accounting"): the class of each lease, so
  farm reruns are charged to the farm.

## 8. Alerting

The rule: **every capability or node the farm takes out of service raises an alert
with the exact fix; nothing is removed silently, and nothing waits for a person to
recover.** This needs `kbf-alert` built.

| Alert | Fires when | Says | Resolves when |
|---|---|---|---|
| Capability withdrawn | a probe fails | node, capability, probe output, fix command | the probe passes |
| Farm fault without a probe | a driver or control fault | node, reason, count in the window | the window passes clean |
| Node suspicion | section 6.3's ratio over a threshold | node, digests that passed elsewhere | the ratio drops |
| Farm verdict memoised | section 6.5's memo used | action, nodes tried, reasons | memo expires |
| Lost blob | section 4.3 | digest, who uploaded it, when | never by itself: storage needs a look |

Alerts deduplicate by node and capability. A node is cordoned automatically only when
the probe that failed is node-wide (scratch space, sandbox canary), and the alert says
so; anything narrower withdraws one capability.

## 9. Metrics

No metrics endpoint exists; these are what one would carry, labelled by platform and
node:

- `kbf_results_total{class, reason}`
- `kbf_farm_retries_total{reason}` and attempts per finished operation
- `kbf_capability_ready{capability}` (gauge) and `kbf_probe_runs_total{probe, result}`
- `kbf_signature_matches_total{signature, proven}`
- `kbf_ambiguous_reruns_total{second}` (`same`, `passed`, `other`)
- time from withdrawal to recovery
- **the leak canary**: OK results with exit 69, 126, 127 or 128+N, per node. A node far
  above its peers has a farm fault with no signature yet.

## 10. Test plan

Each test names the defect it catches; each defect gets a planted mutant that must turn
it red.

**Simulation** (`kbf-sim`, new invariants beside
[simulation.md](simulation.md#4-invariants) I1-I15):

1. *A Farm outcome never finishes an operation while attempts remain, and its rerun
   goes to a different node.* Mutant: rerun on the same node; finish on first fault.
2. *Farm attempts are bounded*; then the answer is `INTERNAL` with the last reason.
   Mutant: no bound (the fleet loops).
3. *No grant needs a capability the worker's latest report withdrew.* Mutant: placement
   reads the report from `Hello` only.
4. *A request whose capability was withdrawn everywhere is answered Farm, not
   refused as unservable.* Mutant: the old `FAILED_PRECONDITION` path.
5. *Nothing but an OK result with exit 0 is cached*, across reruns. Mutant: cache a
   rerun's first failure.

**Real processes** (daemon and native driver, fakes like #173's fake `xcodebuild`):

6. *Licence lapses after start.* The fake answers `-license check` with 0, then 69;
   an action exits 69 with the licence text. Expect: the probe reruns, the Xcode is
   withdrawn, the Result is `INTERNAL` with the fix, the alert fires; flip the fake
   back and the Xcode returns. Mutant: trust the signature without the probe;
   classify only at start (#173 as written fails this test).
7. *An action prints the licence text while the licence is fine.* Expect OK, exit 69,
   no withdrawal. Mutant: drop the proving probe.
8. *A SIGKILL from a test helper* during the run is Ambiguous and reruns; kbf's own
   timeout kill is `DEADLINE_EXCEEDED` and does not. Mutant: forget which kills are
   ours.
9. *Container child OOM, parent exits 1*, under a small `memory.max` parent. Expect
   Farm or Action per 6.1, never a plain exit 1. Mutant: check `oom_kill` on 137 only.
10. *Scratch full during setup* (small filesystem as scratch) is Farm with the scratch
    probe failing; *full during the run* matches the signature and is proven.
11. *Every `RuntimeError` variant and every server-side failure* maps to the table in
    section 5; a table-driven test over all variants. Mutant: map one to OK.
12. *Missing blob*: delete the blob from the CAS after the front's check: expect
    `MISSING` to the client; break only the worker's fetch: expect a rerun elsewhere.

**End to end** (`kbf-it` cell with two nodes, one made unfit):

13. buck2 and Bazel against the cell: the action passes via the fit node with the
    `message` naming the reroute; with both unfit, buck2 exits 2 (not 3) and shows the
    fix; Bazel exits 34 (not 1 or 3). This is the test that catches a farm fault leaking
    as an exit code in the clients people use.

## 11. Decisions for the project

1. Adopt the four classes and the invariant of section 1.
2. Signatures are a reviewed allowlist, every Farm verdict proven by a probe (6.4, C).
3. Probes run continuously and withdraw one capability with an alert; recovery is
   automatic (4.1). This means re-detection and mid-session report changes come first.
4. The server reruns farm faults on another node, at most 2 extra attempts, then
   `INTERNAL` with the fix in `status.message`, and a 60 s memo of the verdict (6.5).
5. OOM: booking decides, one rerun at double the booking (6.1, A with B's retry).
6. Timeouts: measure suspension and pressure; a node-caused timeout is Farm (6.2, B).
7. Flakes: record and alert on node suspicion; never rerun Action failures (6.3, B).
8. Request errors: `FAILED_PRECONDITION` except a malformed action (6.7).
9. Should #173 merge as is (start-only, log-only) and be extended, or be reworked to
   the probe model first? Lean: merge it (it fixes the pilot's failure today), and
   build the probe loop and alert as the next change.
10. Order: keep the reason and node (7); then server reruns (#22); then probes and
    signatures; then alerts; then timeout and OOM attribution.
