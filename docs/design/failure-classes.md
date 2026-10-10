# Failure classes: farm, request, action

When an action fails, the person reading the build log needs one answer first: is this
mine to fix, or the farm's? This document defines the classes of failure, what the
farm does today, and what is **proposed** so that a farm problem is never reported as
the action's own failure. Lease handling is in [daemon.md](daemon.md), outcomes and
retries in [scheduler.md](scheduler.md#outcomes), node reports in
[capabilities.md](capabilities.md), the simulation in [simulation.md](simulation.md).

Line numbers are on `main` at the merge of #177; paths are under `crates/`. "Proposed"
means no code yet.

## Decisions of 2026-10-09

Four questions this draft had left open were decided on 2026-10-09. They are recorded
as **decided**; every other decision in sections 6 and 11 is still an option with a
lean. A decision covers only what it says below. The mechanisms this draft proposes
to carry one out (which status code, what an action key is made of, the floor's decay
numbers, the over-cap memo, node suspicion, the memory ladder's step and cap) are not decided: sections 6.1 to 6.3 list
them as **open within the decision**, each with its options, pros and cons, and a lean.

1. **Out of memory is the farm's until no node is large enough** (6.1). Going over
   memory is not always the action's fault: a code change can need more memory than
   the action used before. An out-of-memory kill is Farm: the server reruns at a larger
   memory booking and remembers it; the action's error only when the largest node is
   not enough. The raise is bounded, and remembered per action key so later runs start
   there; the user is told only once the largest node has been tried.
2. **A timeout is the action's** (6.2). The client set the limit. A paused or frozen
   node is not a timeout: it is a farm error, and the operation is requeued.
3. **Flakes are tracked** (6.3): the same action digest observed both passing and
   failing, recorded per digest and shown, never rerun until green.
4. **#173 is extended, not merged start-only** (section 11, decision 4). When a node
   needs a person to act, the farm alerts that person rather than quietly taking the
   node out of service. An Xcode left out is reported with its reason and fix,
   re-checked, and alerted.

## 1. The classes

| Class | Meaning | Whose to fix | Example |
|---|---|---|---|
| **Farm** | The farm could not run the action, or ran it on a node unfit to run it, or with too little memory booked. | the operator, or the farm itself | Xcode licence not accepted; scratch disk full; lost contact; a blob the CAS lost; an out-of-memory kill below the largest node (6.1) |
| **Request** | The action cannot run as written, on any node. | the client | a Command with no arguments; an output over the size limit; a blob never uploaded |
| **Action** | The action ran on a fit node and exited non-zero, ran past the timeout its client set, or needs more memory than any node offers. | the action's author | a compile error; a failed assertion; a crashed test harness; a timeout (6.2) |
| **Ambiguous** | It ran and failed in a way the farm cannot yet attribute. Resolved by one rerun on another node (section 1.1); never the final answer. | resolved to Farm or Action | a SIGKILL or SIGTERM kbf did not send |

**Test failure vs test error** is a split inside Action, and it belongs to the client.
REAPI has one field for it, `ActionResult.exit_code`; the difference between "the
assertion failed" and "the fixture crashed" lives in the test runner's exit codes and
its JUnit XML. The farm passes both through untouched. What the farm owns is the line
between Farm and everything else.

**The invariant.** *A farm-side problem never reaches the client as the action's exit
code.* Equivalently: an `ExecuteResponse` with status OK means a node that was fit for
the action's capabilities ran it to the end. Two corollaries:

- A Request error never reaches the client as `INTERNAL` (the client would wait for
  an operator who has nothing to fix).
- Every non-OK answer says which class, which node and why, in words a person can act
  on, in the one field every client shows (section 5).

### 1.1 How an Ambiguous result resolves

An Ambiguous result is rerun once, on a node other than the one it ran on. The
second run decides:

| Second run | Answer |
|---|---|
| exits 0 | OK with the second run's result; the first run is recorded as a Farm fault on its node (it counts for node suspicion if that is adopted, 6.3 O6) |
| ends the same way (same signal, not sent by kbf) | **Action**: OK with the second run's result, exit 128+N. The action did it to itself |
| ends any other way (another exit code, another signal) | that run is classified on its own, as if it were the first |
| is a Farm fault | the Farm path (6.5) |

The Ambiguous rerun is one of the operation's 2 extra runs (6.5). An action that kills
itself with SIGKILL every time therefore ends OK with exit 137 after exactly 2 runs; it
is never answered `INTERNAL`. Both runs are kept in the digest's outcome history, so an
Ambiguous run followed by a pass is visible as a possible flake (6.3), not erased by the
answer.

## 2. What the code does today

The daemon sends one `Result` per lease: status OK with an `ActionResult` whatever the
exit code, or a non-OK status (`kbf-proto/proto/kbf/worker/v1/worker.proto:240-251`).

- **Daemon** (`kbf-daemon/src/lease.rs:157-182`): `Killed` is `ABORTED`, `Failed` is
  `INTERNAL`, `Invalid` is `INVALID_ARGUMENT`, `MissingBlob` is `FAILED_PRECONDITION`
  with a `MISSING` violation, `TimedOut` is `DEADLINE_EXCEEDED`, `OutOfMemory` is
  `RESOURCE_EXHAUSTED`. Before a run it may refuse `FAILED_PRECONDITION` (lease kind not
  served, `lease.rs:69-75`) or `UNAVAILABLE` (contact lost, `daemon.rs:468-475`). Its log
  says only `ok=false` (`lease.rs:119`).
- **Server** (`kbf-server/src/farm.rs:518-557`): OK with stored outputs is `Completed`;
  `DEADLINE_EXCEEDED` is `Timeout`; `INVALID_ARGUMENT` keeps its message; **every other
  code is `Failure::Infra` and the daemon's message is dropped** (the catch-all arm,
  `farm.rs:553-556`). The log has the code only (`farm.rs:554`).
- **Answer** (`settle`, `farm.rs:839-871`): Infra becomes `INTERNAL "the farm could not
  run the action"` (`farm.rs:861-864`), the same fixed text for every cause, with no
  node name.
- **Front, before execution**: inputs not in the CAS are answered by `missing()`
  (`kbf-front/src/execution.rs:480-503`): `FAILED_PRECONDITION` with exactly one detail,
  a `PreconditionFailure` with one `MISSING` violation per blob. That is the shape
  Bazel needs to re-upload (section 3); keep it.
- **Retries**: none. A failed result finishes the operation
  (`kbf-sched/src/scheduler.rs:709-730`), though
  [scheduler.md](scheduler.md#outcomes) plans bounded retries (#22). Only a lease
  given up (silent, replaced, reconnected, not started:
  `kbf-sched/src/requeue.rs:28-41`) runs again, with no limit.
- **Cache**: written only for exit 0 and not `do_not_cache` (`farm.rs:849-850`).
- **Exit codes**: the native driver folds a signal into 128+N (`exit_code`,
  `kbf-driver-native/src/runtime.rs:536-541`) before anything classifies it, so a
  SIGKILL and `exit(137)` are the same value by the time the daemon sees them.
- **Readiness**: the node report is detected once, at daemon start; re-detection is
  planned ([capabilities.md](capabilities.md), "Planned"). On macOS an Xcode is
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
   `kill`, a session teardown. Indistinguishable from `exit(137)` today (above).
4. A kernel OOM kill of a child in a container whose parent exits with its own code.
   Only exit 137 with a counted `oom_kill` is caught
   (`kbf-driver-container/src/runtime.rs:300-311`).
5. Scratch disk full while the action runs.
6. Environment drift: a missing tool on `PATH`, the wrong `DEVELOPER_DIR`, leftovers
   of another lease.

A paused or suspended node is not on this list: it does not turn into a timeout
(6.2). Its gap is a different one, #167: a daemon frozen while the machine runs cannot
kill its action, which keeps running beside the requeued copy.

### And the other way round

- Program not found (`kbf-driver-native/src/runtime.rs:172`), an output over the size
  limit (`runtime.rs:636`), and a missing input blob at the worker all reach the client
  as the fixed `INTERNAL`. The `MISSING` detail is dropped at `farm.rs:553-556`, so Bazel is
  never told to re-upload.
- The named Xcode missing on the node is `RuntimeError::Failed`; the comment at
  `xcode.rs:190` says "the action is run elsewhere", but nothing reruns it.
- Native over-booking OOM is `RESOURCE_EXHAUSTED`; the comment at
  `kbf-daemon/src/runtime.rs:46-49` says "the farm's to retry", but nothing does.
- The module comment at `kbf-front/src/execution.rs:20-22` says shutdown `UNAVAILABLE`
  is something "REAPI clients retry". Bazel does; buck2 does not retry inside a stream
  (section 3). The comment should say so.

### What the open pull requests change

- **#173** runs `xcodebuild -license check` and `xcrun --find clang` at daemon start and
  leaves out an Xcode that fails. As first written it checks once, logs, and raises no
  alert: actions then wait for the unservable bound and are refused, or capacity
  quietly shrinks. It is being extended before it merges (section 11, decision 4). It
  touches `discover`, as #172 does; expect a conflict.
- **#172** fixes the sandbox denials for temp items and the `xcrun` cache. #161
  (orphaned runs) and #174 (retention) change nothing a client sees.
- Merged: **#176** ends open Execute streams `UNAVAILABLE` on shutdown; **#177** names
  the worker on OK results only, and logs each requeue with its reason.

None adds retries, keeps the daemon's reason, alerts, or classifies after a run.

## 3. What the clients do with each answer

Read from the source of buck2 (`main`), Bazel (`master`) and the REAPI proto on the
day this was written; not yet confirmed against pinned releases (section 10.3 pins them).
The parts that decide the design:

| Answer | buck2 | Bazel |
|---|---|---|
| OK, exit non-zero | user error, exit 3, never retried; **except** that stderr containing "out of memory", "input/output error", "transport endpoint is not connected" or "dotslash error:" is retagged as infra/environment | `NON_ZERO_EXIT`, user error; test `FAILED`; never retried |
| `INTERNAL` in the response | INFRA, exit 2; `status.message` shown; never retried | retried as a fresh Execute (`--remote_retries`, default 5), then exit 34 `REMOTE_ERROR` |
| `UNAVAILABLE` in the response | INFRA, exit 2; never retried | retried (WaitExecution, then a fresh Execute on `NOT_FOUND`), then **catastrophic**: stops the build even with `--keep_going` |
| `FAILED_PRECONDITION`, every violation `MISSING`, no other detail but DebugInfo, Help, LocalizedMessage, RequestInfo or ResourceInfo | USER, exit 3; **no re-upload** | re-uploads and re-Executes |
| `FAILED_PRECONDITION`, anything else | USER, exit 3 | not retried, exit 34 `REMOTE_ERROR` |
| `DEADLINE_EXCEEDED` in the response | ENVIRONMENT, exit 2; result dropped, so no partial output, whoever caused the timeout | rewritten to `TIMEOUT`; partial outputs downloaded, `ExecuteResponse.message` shown |
| `RESOURCE_EXHAUSTED` | USER, exit 2 | retried |
| `INVALID_ARGUMENT` | INFRA, exit 2 | not retried, exit 34 `REMOTE_ERROR` |
| any of `UNAVAILABLE`, `ABORTED`, `RESOURCE_EXHAUSTED` as the **immediate** status of the Execute call (no Operation sent) | retried (only the stream opening is) | retried |

Where text appears:

| Field | buck2 | Bazel |
|---|---|---|
| `status.message` on a non-OK answer | shown; **the only text that survives** (the `ExecuteResponse` result, `message` and `server_logs` are dropped) | shown |
| `ExecuteResponse.message` on a failed action (OK, exit non-zero) | printed as "Info:" after the output | printed by default (`--remote_print_execution_messages=failure`) |
| `ExecuteResponse.message` on a success | event log only (`buck2 log show`: `CommandExecutionDetails.additional_message`) | only with `--remote_print_execution_messages=success` or `all` |
| `server_logs` | never | fetched on failure; path shown only with `--verbose_failures` |
| `worker`, `auxiliary_metadata` | never | never |

Facts that follow:

1. **buck2 never retries a failure inside the Execute stream**, only opening it. A farm
   fault found after a run must be rerun by the server.
2. **Anything a buck2 user must see on a non-OK answer goes in `status.message`.**
   `ExecuteResponse.message` is for OK answers only.
3. **`FAILED_PRECONDITION` is the wrong code for a farm fault**: buck2 calls it a user
   error. REAPI names it for "no worker available"; kbf departs from the spec here on
   purpose (section 4.1).
4. **Bazel exits 34 for Request and Farm alike** (`FAILED_PRECONDITION`,
   `INVALID_ARGUMENT` and `INTERNAL` after retries). A Bazel user tells them apart only
   from the message, never from the exit code.
5. **Any extra detail on a `MISSING` answer stops Bazel re-uploading.** A detail of a
   type outside Bazel's list (ErrorInfo included) makes the error permanent.
6. **buck2 has no tier for an action's own timeout.** Every `DEADLINE_EXCEEDED` is
   ENVIRONMENT, and the partial output never reaches buck2 (#45 helps Bazel only).

## 4. Detection (proposed)

Three layers: probes before placement, signatures after a run, and the errors the farm
already sees.

### 4.1 Readiness probes, before placement

A **probe** proves the node can use a capability it advertises. Each capability that
needs more than "the file exists" names its probe:

| Capability | Probe | Fix text on failure |
|---|---|---|
| an Xcode build | `xcodebuild -license check`, `xcrun --find clang` (as #173) | `sudo <that Xcode's developer dir>/usr/bin/xcodebuild -license accept`, or the MDM route of [mac-node-provisioning.md](mac-node-provisioning.md) |
| Metal in that Xcode | `xcrun --find metal` | `<developer dir>/usr/bin/xcodebuild -downloadComponent MetalToolchain` (Xcode 26 and later) |
| each SDK the node reports | `xcrun --sdk <sdk> --show-sdk-path` | install the platform in that Xcode |
| the sandbox profile | a canary action run under the profile: writes its temp dir, runs `xcrun --find clang` | names the denied operation |
| scratch space | free bytes and inodes above a floor | names the directory and the floor |
| container images | the image check the driver already does | names the image |

The fix text is a literal command, with the developer directory of the Xcode that
failed spelled out, not `xcode-select`'s choice. Two items are UNVERIFIED and must be
checked on a pilot Mac before the text is pinned in a test: that invoking an Xcode's own
`xcodebuild` by path accepts that Xcode's licence, and the Metal component command.

**How a probe runs.**

- **In the action's context**: the same sandbox profile, user, environment and
  `DEVELOPER_DIR` a lease of that capability gets. A probe run outside the sandbox
  passes on #172's case (`xcrun` denied inside, fine outside) and the leak continues.
- **Bounded**: 60 s. A probe that times out fails, with "probe timed out" as its
  output (`xcodebuild` can hang on a first-launch prompt).
- **One at a time** per node and capability. Leases that need the same probe while it
  runs wait for its verdict; they do not start another.
- **On an injectable clock.** The period, the bound and the hysteresis below read the
  daemon's clock seam; tests trigger a probe through the operator's request and never
  sleep.

**When.** At start, every 10 minutes (the period capabilities.md plans for
re-detection), at once after a proven or unattributed farm fault on that node, and on
an operator's request.

**Hysteresis.** A capability is withdrawn on the first failing probe (a probe is
proof). It returns after **2 consecutive passes at least 60 s apart**. A capability
that returns and fails again within an hour reopens the same alert (section 8).

**What a failing probe does.**

- Withdraws **only that capability** from the node's report (the node keeps serving
  everything else), and sends the changed report mid-session; this needs the planned
  report-change handling (capabilities.md, "Report changes noticed by hash").
- Leaves running leases that use the capability alone. Their results are classified
  as usual; a failure that matches a signature finds the probe already failing and is
  Farm.
- Raises an alert with the fix text (section 8).

**Placement when a capability is withdrawn.** The scheduler keeps a capability a probe
withdrew apart from one no node ever had.

- Some node that is fit and advertises the capability exists, busy or not: the request
  waits, as today.
- No such node, and at least one node has it withdrawn by a probe: the request is
  answered **at once** as a Farm fault, `INTERNAL`, naming the nodes and the fix. It
  does not wait for the unservable bound: Bazel retries `INTERNAL` as a fresh Execute up
  to 5 times, so a 300 s wait would become 30 minutes per action. The verdict memo
  (6.5) answers those retries without placing anything.
- No node ever had it: refused `FAILED_PRECONDITION` after the unservable wait, as
  today (`farm.rs:825`).

This departs from REAPI, which names `FAILED_PRECONDITION` for "no worker available":
buck2 tags that code USER, which would tell the user a broken node is their fault.
Simulation invariant I10 ("refused only for a stated reason") is amended to match: a
refusal is either the unservable verdict after the wait, or a Farm answer when every
node that could serve has the capability withdrawn at that tick. The amendment lands in
[simulation.md](simulation.md) with the code.

### 4.2 Known signatures, after a run

A **signature** is a rule: platform, exit code, a fixed substring of the **last 64 KiB
of stderr**, and the probe that proves it. Signatures are data in the repository,
reviewed like code. The first set:

| Platform | Exit | Stderr contains | Proving probe |
|---|---|---|---|
| macOS | any | `agreed to the Xcode license` | licence check |
| macOS | any | `xcrun: error: unable to find utility` | `xcrun --find <that utility>` |
| macOS | any | `cannot execute tool 'metal'` | Metal probe |
| any | any | `No space left on device` | scratch space |

The licence rule takes any exit code: wrappers (swift-driver, scripts, build-tool
wrappers) often turn `xcodebuild`'s 69 into 1, and the probe is the proof anyway.

When a run exits non-zero and matches a signature, the daemon runs the proving probe
(in the action's context, bounded, shared as in 4.1) before it reports. **The
signature is a hint; the probe is the proof.**

- Probe fails: the result is a Farm fault, the capability is withdrawn, the alert
  fires.
- Probe passes: the result is an ordinary Action result, and a counter records the
  unproven match. A test that prints the licence message on purpose, or a tool that
  wraps `xcodebuild` and exits 69 for its own reason, is never rerouted.

The lease keeps its booking while the probe runs, and the daemon keeps listing it as
running, so the fence timers see a run that took up to 60 s longer.

One known gap: if the operator fixes the node between the failed run and the probe,
the probe passes and the user sees the exit 69 once. The unproven-match counter records
it; the gap is accepted.

### 4.3 Errors the farm sees itself

These need no guessing, only keeping the reason the daemon already writes:

- **Signals are classified before they become an exit code.** The driver reports
  "exited with N" and "ended by signal S" as different values; 128+N is computed only
  when the `ActionResult` is written. `exit(137)` is Action, never rerun.
- **Who sent the signal.** The daemon knows each kill it makes (timeout, cancel, fence,
  the memory watch). Any other SIGKILL or SIGTERM is Ambiguous (section 1.1). A signal the
  kernel raises on the program's own fault (SIGSEGV, SIGABRT, SIGBUS, SIGILL, SIGFPE) is
  Action. A SIGSEGV someone else sent with `kill` cannot be told apart, and is accepted
  as Action.
- **Driver errors** (`RuntimeError::Failed`): setup, cgroup, podman, CAS I/O, clean.
  Farm. A clean that fails after a good run (`kbf-driver-native/src/runtime.rs:352`)
  keeps the good result and withdraws the node's scratch capability instead of
  discarding the work.
- **Kernel OOM.** Read the lease cgroup's `oom_kill` count for every exit, not only
  137 (section 6.1).
- **Control faults**: fence (`ABORTED`), contact lost (`UNAVAILABLE`), lease kind not
  served, the named Xcode absent, outputs not stored (`farm.rs:539-542`). Farm.
- **Request errors**: no arguments, image by tag, output path in the input root
  (today's `Invalid`); an output or stdout over the size limit; a blob the client never
  uploaded.
- **Missing blob at the worker.** The front already checked the inputs, so a blob the
  worker cannot find is either lost by the CAS or a fetch error. The server checks the
  CAS:
  - absent: answer exactly as `missing()` does, `FAILED_PRECONDITION` with only the
    `PreconditionFailure` detail. Bazel re-uploads and re-Executes. buck2 shows a user
    error, exit 3, and does not re-upload; the message says the farm lost the blob and
    a rebuild will upload it. The loss is counted as a farm fault and alerted.
  - present: a fetch error on that node. Farm, rerun elsewhere.

## 5. What the client receives (proposed)

The rule from section 3: on a non-OK answer every word a person needs is in
`status.message`; `ExecuteResponse.message` is used on OK answers only.

| Class | Status | Exit code | Text, and where it is visible | Cached |
|---|---|---|---|---|
| Action | OK | the action's | `ExecuteResponse.message` names the node. On a failure both clients print it; on a success buck2 keeps it in the event log and Bazel prints it only with `--remote_print_execution_messages=success` | exit 0 only, as today |
| Ambiguous, resolved | OK | the deciding run's (section 1.1) | `ExecuteResponse.message`: "ran twice: ended by <signal> on A, then <outcome> on B". Visible as for Action | as Action |
| Action, timeout | `DEADLINE_EXCEEDED`, with the partial result (#45) | none | `status.message`: node and timeout. buck2 shows it as ENVIRONMENT and drops the partial result; Bazel shows it and the partial outputs | no |
| Action, needs more memory than any node offers (6.1) | `FAILED_PRECONDITION` (the lean of 6.1, O1) | none | `status.message`: "kbf: the action needs more memory than any node offers: killed at <used> GiB with <booked> GiB booked on <node>, the largest node for its platform. Runs: n (<bookings>)." buck2 USER, exit 3, as for a failed action; Bazel exit 34, not retried | no |
| Farm, rerun succeeded | OK | the action's | `ExecuteResponse.message`: "ran on B after a farm fault on A: <reason>", or "ran with <N> GiB booked after an out-of-memory kill at <M> GiB on A". Visible as for Action | as Action |
| Farm, runs used up, or the capability withdrawn everywhere | `INTERNAL` | none | `status.message`: "kbf farm fault on <node>: <reason>. Operator fix: <fix>. Runs: n." Both clients show it | no |
| Request | `FAILED_PRECONDITION`, or `INVALID_ARGUMENT` for a malformed action (6.7) | none | `status.message`: the limit or field at fault | no |
| Missing blob | `FAILED_PRECONDITION` + `MISSING`, nothing else | none | `status.message`: the blobs | no |
| Server shutting down | `UNAVAILABLE` (#176) | none | `status.message`, as today | no |

**Details.** A non-OK answer other than a `MISSING` one also carries a
`google.rpc.ErrorInfo` with domain `kbf` (the front already uses one, reason
`NO_WORKER_CAN_RUN`, in a queued operation's metadata: the reason is
`kbf-front/src/execution.rs:92`, the `ErrorInfo` is built at 521-527), reason
`FARM_FAULT`,
`REQUEST_ERROR`, `ACTION_TIMEOUT` or `ACTION_OUT_OF_MEMORY`, and metadata `node`,
`signature`, `probe`, `runs`.
A `MISSING` answer carries the `PreconditionFailure` and nothing else (section 3, fact
5). No client reads the ErrorInfo today; it is for our UI, our CLI and any client that
learns to.

**Server restarts under Bazel.** On `UNAVAILABLE` Bazel retries WaitExecution, gets
`NOT_FOUND` from the new process, then re-Executes. A restart longer than Bazel's
retry backoff (a few seconds at the defaults) ends a Bazel build catastrophically, even
with `--keep_going`. A rolling server restart must therefore keep a server answering
throughout (scheduler.md, "More than one server"); a single server's restart will stop
running Bazel builds. buck2 builds fail the open actions as INFRA either way.

**Deliberate departures from the spec's wording**, recorded so no one "fixes" them:

- Withdrawn-everywhere is `INTERNAL`, not `FAILED_PRECONDITION` (4.1).
- If the lean of 6.1 (O1) is taken, needing more memory than any node offers is
  `FAILED_PRECONDITION`, not `RESOURCE_EXHAUSTED`: the spec's quota wording fits the
  latter, but Bazel would retry it five times at the same size, and buck2 would exit 2
  instead of a failed action's 3.
- Program not found inside the input root is answered as an `ActionResult` with exit
  127 that the program never produced (6.7). It is safe only because a non-zero exit is
  never cached.

## 6. Decisions: decided and open

6.1 to 6.3 are **decided** (decisions of 2026-10-09) in what their **Decision**
paragraph says. What each lists as **open within the decision** is a proposal, with
options and a lean. 6.4 to 6.7 are open, with leans.

### 6.1 Out of memory (DECIDED, with open parts)

**Decision.** An out-of-memory kill is Farm. A change to the code can legitimately need
more memory than the action used before, so the farm reruns it with a higher memory
ask, bounded, and remembers the raised ask per action key so later runs start there; it
is the action's, and the user is told it needs more memory than any node has, only when
the largest node is not enough. The rest of this section is how this draft proposes to
carry that out; the parts the decision does not settle are marked **open** (O1 to O5,
and O7 for the ladder's step, cap and bound) with their options and a lean. The draft's "booking decides"
attribution is dropped: whether the run was over or under its booking, and who set the
booking (the client's `kbf-book-mem-gib`, the default, a learned size), no longer
change the class.

**What counts as an out-of-memory kill.** The native driver's memory watch killing the
tree past the lease's limit (`kbf-driver-native/src/runtime.rs:247-249`), or a kernel
OOM kill counted in the lease cgroup's `oom_kill`, read on every exit code (4.3). A
SIGKILL kbf did not send and cannot tie to memory (a macOS memory-pressure kill with no
record of it) stays Ambiguous (1.1). The container driver caps each lease at the
native driver's limit (`memory.max`, [daemon.md](daemon.md), "Cgroups and limits") and
tells a kill at that cap (`MEMORY_KILL_OUT_OF_MEMORY`) from a kill by the node's
`actions/` limit or the host (`MEMORY_KILL_BUSY_NODE`, which says nothing about the
booking) by the lease cgroup's `memory.events`.

**The ladder.** Decided: each out-of-memory rerun books more memory than the run before
it, the raise is bounded, it stops at the largest node (the **cap**), and only a kill
at the cap reaches the user. Open (O7): how much each rung adds, which nodes set the
cap, and how the ladder counts against the 6.5 budget. The rest of this paragraph is
the lean of O7. The server reruns the operation with its memory booking doubled, in
whole GiB, up to the cap: the largest memory of any node, live or cordoned at the
time of the kill, whose capabilities fit the action's platform. A doubling that would
pass the cap books the cap; the run at the cap is the last rung.

- **Its own bound, not the 6.5 budget.** Counting the ladder toward the 3-run budget
  would answer the action's error at four times the first booking, far below the
  largest node, which is what the decision rejects. The ladder is bounded by itself: from
  booking `b` to cap `c` it is at most ceil(log2(c / b)) reruns, 9 from the 1 GiB
  default to a 512 GiB node. The bookings double, so the whole ladder books less
  memory than two runs at the cap, and a killed run usually ends early.
- Farm and Ambiguous results during the ladder use the 6.5 budget as usual, so an
  operation runs at most 3 times plus the ladder's length.
- A rung may run on the same node: a memory kill says nothing against the node. Whether
  a kill below the run's own booking counts against the node is open (O5).
- A rung waits for room like any request. The cap is taken from live and cordoned
  nodes, so a rung never books more than a node the scheduler knows of could give it.

**The remembered booking.** The booking of the rung that passed is kept as a **memory
floor** for the action's key, and later requests at that key book at least the floor
(never less than they ask).

- **Where it is kept.** In the scheduler's sizing state, as the first piece of the
  planned learned sizes ([scheduler.md](scheduler.md), "Learned sizes": "raised quickly
  after an overshoot and lowered slowly"). Today the scheduler's state lives only in the
  server process, so a server restart forgets the floors; that costs one more
  out-of-memory run per key, never a wrong answer. With the replicated control log
  (scheduler.md, "More than one server") floors are committed records like any other.

**When the cap is not enough.** A kill at the cap is the action's (decided). Its
`status.message` carries the text of section 5 and the last run's stderr tail goes
into `server_logs`; the status code is open (O1).

**O1, open: the status code past the cap.** The decision fixes what the user is told,
not the code.

- **A. `FAILED_PRECONDITION`** with an `ErrorInfo` of reason `ACTION_OUT_OF_MEMORY`.
  Pro: buck2 shows it as USER, exit 3, the same exit as a failed action; Bazel does not
  retry it and exits 34; both show `status.message`. Con: departs from the spec's
  wording (section 5), and Bazel's exit 34 is shared with Farm and Request.
- **B. `RESOURCE_EXHAUSTED`.** Pro: the spec's quota wording fits it. Con: Bazel
  retries it 5 times at the same size, and buck2 exits 2, an infrastructure tier
  (section 3).
- **C. OK with exit 137.** Pro: both clients show it as an ordinary failed action. Con:
  the program produced no exit code; a made-up 128+9 reads in a test runner as a crash,
  not as "needs a bigger node", and breaks the invariant that OK means the run went to
  its end.

**Lean: A.** Settled by section 10.3's pinned clients confirming the exits of section 3.

**O2, open: what the action key is made of.** The decision says "per action key"; what
a key is, is this draft's reading. The key is not the digest: the case the decision
describes, a code change, makes a new digest, so a floor kept per digest would never
help the next commit.

- **A. Derived from the request alone**: the platform properties, the Command's output
  paths and its first argument. Pro: needs nothing from the client; a code change keeps
  all three. Con: two distinct actions with the same outputs and tool share a floor; a
  rule that renames its outputs per configuration loses its floor.
- **B. A, plus REAPI `RequestMetadata`** (`action_mnemonic`, `target_id`,
  `configuration_id`) where the request carries it. Pro: names the target as a person
  would; fewer accidental shares. Con: which of these buck2 sends is UNVERIFIED, and a
  key that changes with the client's metadata splits floors between buck2 and Bazel.
- **C. `RequestMetadata` only.** Pro: simplest to explain. Con: a request without it has
  no key, so no floor.

**Lean: B.** Settled by reading what the pinned buck2 and Bazel send.

**O3, open: floor decay and reset.** A floor that never falls keeps over-booking after
the code shrinks again.

- **A. Halve and expire**: a floor halves (never below the default booking) after 20
  consecutive passing runs at the key whose measured peak stayed under half of it, and
  is dropped after 30 days with no run at the key; an operator can clear one key or all
  through the server's API (planned). Pro: bounded waste, simple. Con: the numbers are
  guesses until pilot data sets them; decay needs each driver to report the action's
  whole-tree peak (the native watch measures it, the container driver can read the lease
  cgroup's `memory.peak`; today's `ResourceUsage` holds one process's peak only).
- **B. No decay; operator clears only.** Pro: no peak reporting needed. Con: floors
  only grow, and capacity is lost silently.
- **C. Track the measured peak** (the planned learned sizes) instead of a floor. Pro:
  one mechanism. Con: depends on learned sizes, which are not built.

**Lean: A**, with the numbers set from pilot data, folded into C when learned sizes
land.

**O4, open: a repeat past the cap.** Once a key is past the cap, a Bazel retry or a
rebuild of the same digest would climb the ladder again.

- **A. An over-cap memo**: the key's floor records "over the cap of <c> GiB"; a repeat
  of the same digest is answered at once, without a run, while the cap is unchanged (a
  larger node registering clears it; `skip_cache_lookup` bypasses it). A new digest at
  the key starts at the cap and runs once. Pro: Bazel's 5 retries cost nothing. Con: a
  new mechanism beside the 6.5 verdict memo; an answer given without a run.
- **B. No memo**: every repeat climbs the ladder. Pro: nothing to clear. Con: up to
  ceil(log2(c / b)) + 1 runs per retry, 6 times over under Bazel.
- **C. No memo, but the floor makes a repeat start at the cap.** Pro: one run per
  retry, no new mechanism. Con: still one full run at the largest node per retry.

**Lean: A**, sharing the 6.5 memo's clearing rules.

**O5, open: a kill below the run's own booking.** A run killed while using less than it
booked means the node ran out, not the action.

- **A. Count it as a farm fault on that node** (section 8's alert). Pro: finds nodes
  whose free memory is wrong. Con: depends on a whole-tree peak at the kill, which only
  the native watch measures today.
- **B. Treat it as any other memory kill.** Pro: simpler. Con: a node that over-commits
  stays hidden.

**Lean: A** where the peak is measured, B elsewhere.

**O7, open: the ladder's step, cap and bound.** The decision fixes that the ask is
raised, bounded, and stops at the largest node; not by how much each rung raises it,
which nodes set the cap, or how the ladder counts.

- **Step.**
  - **A. Double, in whole GiB.** Pro: at most ceil(log2(c / b)) reruns; the whole
    ladder books less than two runs at the cap. Con: a rung can book up to twice what
    the action needs, and the floor keeps that until it decays (O3).
  - **B. Add a fixed amount** (for example the first booking) per rung. Pro: the floor
    lands close to the need. Con: linear in the gap: 511 reruns from 1 GiB to a
    512 GiB node.
  - **C. Go straight to the cap** after the first kill. Pro: one rerun. Con: every
    memory kill books the largest node, and its floor keeps booking it, which crowds
    placement on the largest nodes.
- **Cap.**
  - **A. The largest node, live or cordoned at the kill, whose capabilities fit the
    platform.** Pro: a node cordoned for maintenance still counts, so the action is
    not told "more than any node" while that node is away. Con: a rung that only a
    cordoned node fits waits for it.
  - **B. Live nodes only.** Pro: a rung never waits on a cordoned node. Con: while the
    largest node is cordoned, a kill below its size reaches the user as the action's.
  - **C. An operator-set cap per platform.** Pro: predictable. Con: drifts from the
    nodes actually registered.
- **Bound.**
  - **A. Its own bound**, apart from the 3-run budget of 6.5 (as above). Pro: the user
    is told only at the cap, as decided. Con: an operation can run 3 times plus the
    ladder's length.
  - **B. Count rungs in the 3-run budget.** Pro: one bound. Con: answers the action's
    error at four times the first booking, below the largest node, against the
    decision; listed only to record why it is not taken.

**Lean: A, A, A** (doubling; the largest live or cordoned fit node; its own bound).
Settled by pilot data on how far past their booking killed actions go.

### 6.2 Timeouts (DECIDED)

**Decision.** An action's timeout is Action: the client set it (`Action.timeout`, from
the buck2 rule or Bazel's test size). It is answered `DEADLINE_EXCEEDED` with the
partial result (#45) and `ACTION_TIMEOUT`, and never rerun. buck2 tags every
`DEADLINE_EXCEEDED` ENVIRONMENT and drops the partial result (section 3, fact 6); that
is the client's tiering, and `status.message` says the action's own timeout was reached.

**A paused or frozen node is not a timeout.** It is a farm error, and the operation is
requeued (decided). Today only half of that holds:

- **The machine suspended.** The drivers' timeout timers run on tokio's monotonic
  `Instant`, which does not advance while the machine sleeps, while the fence clock
  counts suspension (`CLOCK_BOOTTIME`, `mach_continuous_time`; [daemon.md](daemon.md)).
  A suspend past T = 40 s therefore ends in the fence: the run is killed and reported
  `ABORTED`. Past G = 60 s the server has already given the lease up and requeued the
  operation (`kbf-sched/src/requeue.rs`), outside the rerun budget, and the late result
  loses (simulation I5). On resume the fence is checked first (simulation F2.6).
- **Between T and G, today.** The server still holds the lease, so it accepts the
  `ABORTED` result: `farm.rs:553-556` maps it to `Failure::Infra`, and the scheduler
  finishes the operation (`kbf-sched/src/scheduler.rs:719-728`). The client gets the
  fixed `INTERNAL` once and nothing is requeued (simulation F2.2: "retrying it is #22,
  planned"). Requeuing it needs the server reruns of 6.5, which are **open** (section
  11, item 9); until they land, the decision holds past G only.
- **The daemon stopped, the machine running** (SIGSTOP, a debugger, swap thrash). A
  native action runs on in its own process group, so a timeout it reaches is its own
  wall time. Past G the server requeues. The frozen daemon cannot kill its run, which
  keeps running beside the requeued copy until the daemon resumes and fences it:
  **#167** (open). That is a duplicate-run gap, not a misattributed timeout.

Attributing timeouts by node pressure is dropped: placement never books more than a
node's capacity, so a slow action on a fit node is the action's. Per-lease suspended
and stopped time and Linux PSI may come back as metrics (section 9), never as a verdict.

### 6.3 Flakes (DECIDED, with one open part)

**Decision.** Flaky actions are tracked. **A flaky action is an action digest observed
both succeeding (exit 0) and failing (an Action-class non-zero exit or timeout).**

- **Recorded per digest**: each run's node, class, exit code or signal, time and
  operation, and per digest the counts of passes and failures and when each was first
  and last seen. A bounded table on the server today; a query over the planned usage
  records ([scheduler.md](scheduler.md), "Accounting") once each record carries its
  run's class and outcome.
- **Shown** in the API (`GET /v1/flakes`, planned: digest, counts, nodes, last seen,
  and the target when `RequestMetadata` names it) and in the UI.
- **Never acted on.** No rerun until green, and no rerun of an Action failure to look
  for a flake; the client gets the run's own result. Rerunning Action failures would
  double the cost of every real failure and hide flakes from their authors.

**When a flake is observed.** A pass is cached, so after it the same digest normally
never runs again. A flake is seen when:

1. a failed run (not cached) is followed by a later request for the same digest, which
   runs and passes (typically the next build after a failure);
2. a request bypasses the cache (`skip_cache_lookup`, `do_not_cache`; for example
   Bazel's `--runs_per_test` or `--nocache_test_results`) and runs a digest that passed
   before;
3. within one operation, a farm rerun (6.5) follows a run whose ending looked like the
   action's own.

A pass followed by a failure is seen only in case 2. The record is a lower bound, and
the API and UI say so.

**Farm reruns must not hide a flake.** Every run of an operation is recorded with its
own outcome, not only the run that was answered:

- An Ambiguous run (a signal kbf did not send) followed by a pass is answered OK, and
  its first run counts as a farm fault on its node (1.1). The digest is recorded with
  both runs and marked a **possible flake**; any later Action-class failure of it makes
  it flaky.
- A run a probe proved Farm (4.2) does not count toward flakiness: the probe proved the
  node. Neither does a rung of the memory ladder (6.1).

**O6, open: node suspicion.** Not part of the decision; carried over from the first
draft.

- **A. Alert on a node** whose Action failures pass elsewhere more often than its peers'
  (section 8). Pro: it is how a farm fault with no signature yet gets found. Con: needs
  a threshold and enough traffic per node to compare; a node that serves one team's
  flaky tests looks suspicious.
- **B. A metric only** (section 9's leak canary), no alert. Pro: no threshold to tune
  before pilot data. Con: a person has to look.

**Lean: B first, then A** once pilot data sets the threshold.

### 6.4 Known signatures: allowlist or heuristics

- **A. Allowlist per platform**, each entry with its proving probe and fix text.
- **B. Generic heuristics**: exit 69, 126, 127, "permission denied", "license".
- **C. Both**: allowlist decides Farm; heuristics only count, to suggest new entries.

B misclassifies user failures (stderr is the action's to write) and so reruns them.
**Lean: C**, with every Farm verdict confirmed by a probe (section 4.2).

### 6.5 Who reruns a farm fault

- **A. The server**, on a different node.
- **B. The client**: answer a retryable code and let it retry.

B does nothing for buck2 when the fault is found after a run (buck2 retries only a
refusal at the stream's opening), and Bazel treats `UNAVAILABLE` as catastrophic.
**Lean: A**, with these rules:

- **Budget.** Each operation may run at most **3 times** in total (2 extra runs) after
  Farm or Ambiguous results. The operation keeps its place and its queue time. Leases
  given up for silence, replacement, reconnection or not starting (`requeue.rs`) are
  not results and do not use the budget, as today; bounding those is a separate
  question (#22). Out-of-memory reruns have their own bound, the ladder of 6.1, and
  do not use this budget either (the lean of 6.1, O7).
- **Excluded nodes.** The operation records the nodes whose runs ended Farm or
  Ambiguous and is not placed on them again. If no node outside that set can serve it,
  the answer is given at once, by the class of the last run, with no wait.
- **Answer when the budget is used.** `INTERNAL` with the last run's node, reason and
  fix in `status.message`.
- **The verdict memo.** Bazel retries `INTERNAL` as up to 5 fresh Executes; without a
  memo that is 15 more runs that will fail. The server remembers, for 60 s, the Farm
  verdict for an action digest **together with the faulted capability on each node**,
  and answers a repeat at once. The memo:
  - is cleared for a digest when any of its faulted capabilities passes its probe again
    or the node's report changes, so a rebuild right after the operator's fix runs;
  - is bypassed when the request sets `skip_cache_lookup`;
  - also covers the withdrawn-everywhere answer of 4.1;
  - is server policy, not an action-cache entry: REAPI says a failed result must not
    be cached, and the memo stores no `ActionResult`.
- **What it needs in code.** A `Failure::Farm` outcome the scheduler requeues from
  (today `Failure::Infra` finishes the operation, `kbf-types/src/work.rs:138-147`), the
  excluded nodes and the run count in the operation's state, and the reason kept beside
  the outcome as `Detail::Invalid` keeps its reason today.

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
`FAILED_PRECONDITION` for output over the size limit (and, if the lean of 6.1 O1 is
taken, for an action that needs more memory than any node offers, which is Action, not
Request, and says so in its `ErrorInfo`). Program not found: a path inside the input root is Action (exit 127 with kbf's message on stderr,
as a shell and podman would; a spec departure, section 5); an absolute path outside it,
missing on this node while peers of the same platform have it, is environment drift,
so Farm.

**Done for program not found** (#275, both drivers). Container: the image is pinned by
digest and the environment is the Command's alone, so when crun reports it cannot find
or execute `argv[0]` the answer is the action's exit 127 or 126, kbf's message (the
program and the Command's `PATH`, or that it sets none) then Podman's words on stderr.
Only crun's own lookup report counts, not Podman's wrapping, which it also puts on mount
and cgroup faults. Native: a relative path, or a bare name on a `PATH` of relative
entries only, is the action's 127 or 126; an absolute path, an absolute `PATH` entry or
the default `PATH` stays Farm, its message naming the `PATH` searched. Telling drift
from a typo by asking peers is not built.

## 7. Labelling: where the class is recorded

- **Client**: section 5's status, message and `ErrorInfo`; `ExecutedActionMetadata.worker`
  on every result, failed ones included (in the partial result where there is one).
  Neither client shows `worker` today, so the node also goes in the text.
- **Server log**: one line per run with operation, lease, node, class, reason,
  signature and fix. Today it has the code only (`farm.rs:554`).
- **Daemon log**: the reason and class beside `lease finished` (`lease.rs:119`).
- **`server_logs`**: a run history blob per operation (each run's node, class, reason,
  stderr tail). Bazel fetches it on failure; the UI reads it.
- **`/v1/nodes`**: per node, each capability's probe state (ready, withdrawn with
  reason and fix, since when) and the last farm faults. The extended #173 starts this
  with the Xcodes it leaves out (section 11, decision 4).
- **`/v1/flakes`** (planned): the flake record of 6.3.
- **Usage records** (planned in scheduler.md, "Accounting"): the class of each lease, so
  farm reruns are charged to the farm.

## 8. Alerting

The rule: **every capability or node the farm takes out of service raises an alert
with the exact fix; nothing is removed silently, and the farm never waits for a person
to resume serving.** An alert may wait for a person; service does not. This needs
`kbf-alert` built, and its windows read an injectable clock.

| Alert | Fires when | Says | Resolves when |
|---|---|---|---|
| Capability withdrawn | a probe fails | node, capability, probe output, fix command | the capability returns (4.1 hysteresis) |
| Farm fault without a probe | a driver or control fault, or an out-of-memory kill below the run's own booking (6.1, if O5 A is taken) | node, reason, count in the last 15 minutes | 15 minutes pass with no such fault on that node |
| Node suspicion | section 6.3's ratio over a threshold (6.3, O6 A; open) | node, digests that passed elsewhere | the ratio drops below the threshold |
| Farm verdict memoised | section 6.5's memo used | action, nodes tried, reasons | the memo expires or is cleared |
| Lost blob | section 4.3 | digest, when it was uploaded | the operator acknowledges it (service never waited: the client re-uploaded) |

**One alert per cause.** Alerts are keyed by node and capability (or node and reason):
N faults on one key raise one alert with a count. An alert resolved less than an hour
ago that fires again reopens, with its flap count incremented, and notifies again only
after the first reopening in that hour; so a probe that flaps does not flood the
operator.

A node is cordoned automatically only when the probe that failed is node-wide (scratch
space, sandbox canary), and the alert says so; anything narrower withdraws one
capability.

## 9. Metrics

No metrics endpoint exists; these are what one would carry, labelled by platform and
node:

- `kbf_results_total{class, reason}`
- `kbf_farm_reruns_total{reason}` and runs per finished operation
- `kbf_capability_ready{capability}` (gauge) and `kbf_probe_runs_total{probe, result}`
- `kbf_signature_matches_total{signature, proven}`
- `kbf_ambiguous_reruns_total{second}` (`same`, `passed`, `other`)
- `kbf_oom_reruns_total{rung}`, `kbf_oom_over_cap_total`, and the number of memory
  floors held (6.1)
- `kbf_flaky_digests` and `kbf_possible_flaky_digests` (gauges, 6.3)
- possibly later, as metrics only and never as a verdict (6.2): per-lease time the
  machine was suspended or the daemon stopped, and Linux PSI
- time from withdrawal to recovery
- **the leak canary**: OK results with exit 69, 126 or 127, or ended by a signal kbf
  did not send and the kernel did not raise for the program's own fault, per node,
  compared with the node's peers on the same platform. Ordinary crashes (SIGABRT,
  SIGSEGV) are left out. A node far above its peers has a farm fault with no
  signature yet.

## 10. Test plan

Each test says what it proves and names the planted mutant that must turn it red.
Tests 19, 29, 30 and 31 and invariants I17 and I18 pin the leans of 6.1 (O1 to O4 and
O7: the doubling rungs, the cap's nodes, the ladder's own bound, `ACTION_OUT_OF_MEMORY`)
and change if another option is chosen. What they assert of the decision itself stands
under any option: a kill below the cap is rerun with a larger booking, the booking
never passes the cap, and only a kill at the cap is answered as the action's.

### 10.1 Simulation (`kbf-sim`)

New invariants beside [simulation.md](simulation.md#4-invariants) I1-I15:

- **I16. An OK answer comes only from a fit run.** An OK answer is produced only by a
  lease whose run the daemon model marked as on a node fit for the action's
  capabilities.
- **I17. Farm runs are bounded and spread.** No operation runs more than 3 times after
  Farm or Ambiguous results, and no two of those runs are on the same node. Rungs of
  the memory ladder are counted apart (I18; the lean of 6.1, O7).
- **I18. The memory ladder climbs and stops.** Each out-of-memory rerun books more
  memory than the run before it and never more than the cap (6.1, decided); the
  action's out-of-memory answer follows only a kill at the cap (decided). Under the
  leans, each rung doubles (O7) and that answer carries reason `ACTION_OUT_OF_MEMORY`
  (O1).
- **I5 and F2.6, unchanged, now carry 6.2**: a suspend never produces
  `DEADLINE_EXCEEDED`; past T it is a fence, past G a requeue.
- **I10, amended** as in section 4.1.

The daemon model gains a fault: **an unfit node** that exits 69 with the licence text
for every lease needing a given capability, with a probe that fails until a scheduled
recovery time. It joins F2 (as a worker fault) and F3 (as a capability routing case).

| # | Family, generator | Proves | Mutant that turns it red |
|---|---|---|---|
| 1 | F2, the unfit-node fault on one of 2 to 8 workers | I16, I17: a Farm result is rerun, elsewhere | rerun on the same node; finish on the first fault |
| 2 | F2, every worker of the platform unfit | I17, then `INTERNAL` with the last reason | no bound (the fleet loops) |
| 3 | F3, a capability withdrawn mid-session by a report change | no grant needs a capability the worker's latest report withdrew | placement reads the report from `Hello` only |
| 4 | F3.1-style, the only capable worker unfit | answered Farm at once; I10 as amended | the old `FAILED_PRECONDITION` after the unservable wait |
| 5 | F1.7 (dedup) with reruns | nothing but an OK result with exit 0 is cached | cache a rerun's first failure |
| 6 | F3, unfit node recovers at a random time on the virtual clock | after recovery the memo is cleared and a repeat runs | the memo ignores capability recovery |
| 7 | F2, the unfit-node fault | I16 | the step after a run returns the `ActionResult` (OK, exit 69) even when the probe failed |
| 8 | F2, the unfit-node fault | I16 | `farm.rs` `outcome()` returns `Completed` for a result the daemon marked Farm |
| 29 | F2 with a memory fault: each action needs a random size from 1 GiB to above the largest node; nodes of mixed sizes, some cordoned (cordoned nodes set the cap: O7 lean) | I18, I17 | count the ladder in the 3-run budget (answers at 4 GiB); no cap (a rung books more than any node and waits out the unservable bound) |
| 33 | F2.6 with an action timeout shorter than the suspend: a 10 s timeout, a suspend of 50 s (past T) 5 s into the run | `ABORTED` by the fence and requeued (needs the 6.5 reruns; today the fence between T and G is answered `INTERNAL` once, 6.2); never `DEADLINE_EXCEEDED`; no flake recorded | the daemon model's timeout counts suspended time |

### 10.2 Real processes

Daemon and native or container driver, with fakes like #173's fake `xcodebuild`, and
the daemon's clock seam driven by the test.

| # | Setup | Expect | Mutant |
|---|---|---|---|
| 9 | Licence lapses after start: the fake answers `-license check` with 0, then 69; the action exits 69 with the licence text | the probe reruns, the Xcode is withdrawn, the Result is Farm with the exact fix string pinned; flip the fake back, trigger 2 probes 60 s apart on the test clock: the Xcode returns | trust the signature without the probe; check only at start (#173 as first written fails this); recover on a single pass |
| 10 | The action prints the licence text while the licence is fine | OK, exit 69, no withdrawal | drop the proving probe |
| 11 | The licence text placed in stderr just past the last 64 KiB, and just inside it | past: Action; inside: probe runs | search all of stderr, or none of it |
| 12 | A sandbox profile that denies the `xcrun` cache; the action prints `unable to find utility` | Farm (the probe, run in the sandbox, fails too) | run the probe outside the sandbox |
| 13 | A probe that never answers | after 60 s on the test clock, the capability is withdrawn with "probe timed out" | no bound on the probe |
| 14 | Three leases match the same signature at once | one probe runs; all three get its verdict | a probe per lease |
| 15 | The action blocks on a fifo; the test sends SIGKILL to it while it is blocked | Ambiguous, rerun | forget which kills are ours |
| 16 | kbf's own timeout kill | `DEADLINE_EXCEEDED`, no rerun | as 15 |
| 17 | The action calls `exit(137)` | Action, OK exit 137, no rerun | classify on the exit value instead of "ended by a signal" |
| 18 | The action sends itself SIGKILL every time, 2 nodes | OK, exit 137, after exactly 2 runs | count the Ambiguous rerun outside the budget; answer `INTERNAL` |
| 19 | Native: booking 1 GiB, cap (the test's node) 4 GiB, the action needs 1.5 GiB. Container: a child is OOM-killed under the test's `actions/` limit while its parent exits 1 | native: one rerun at 2 GiB (doubling, the O7 lean; under any step, one rerun above 1 GiB), OK exit 0, the message names both runs. Container: classified out of memory, never OK exit 1 | rerun at the same booking; check `oom_kill` on 137 only |
| 20 | Scratch full during setup (a small filesystem as scratch); full during the run | setup: Farm with the scratch probe failing; run: signature matched and proven | treat setup errors as Action |
| 21 | Every `RuntimeError` variant and every server-side failure, table-driven; the mapping is a `match` with no wildcard arm, so a new variant does not compile until it is classified | the class and code of section 5 | `Failed` mapped to `INVALID_ARGUMENT`; `MissingBlob` mapped to `INTERNAL` (today's defect) |
| 22 | A `MISSING` answer, from the front and from the server's re-check | exactly one detail, the `PreconditionFailure` | add an ErrorInfo to it |
| 23 | Missing blob: delete it from the CAS after the front's check; separately, break only the worker's fetch | `MISSING` to the client; a rerun elsewhere | the server answers `INTERNAL` for both |
| 24 | Alerts: 5 faults on one node and capability; the probe passes twice; the probe flaps 4 times in an hour | 1 alert with count 5; resolved; one reopened alert, notified once | key alerts per fault; no flap rule |
| 30 | Memory floor: digest A at key K is killed at 1 GiB and passes at 2 GiB; then digest B at K (a changed input); then 20 passes at K peaking under 1 GiB; then 30 days without a run, on the test clock | B books 2 GiB at its first run; the floor then halves to 1 GiB; then K is dropped | keep the floor per digest (B starts at 1 GiB); never lower it |
| 31 | The action needs more than the cap (4 GiB) | ladder 1, 2, 4 GiB (O7 lean), then `FAILED_PRECONDITION` with `ACTION_OUT_OF_MEMORY` (O1 lean) and the exact text of section 5; a repeat of the digest is answered without a run; with `skip_cache_lookup` it runs | answer OK exit 137; answer `RESOURCE_EXHAUSTED`; no memo, so a repeat climbs the ladder again |
| 32 | Flakes: a digest fails, then a second request for it passes; separately, an operation's first run ends by a SIGKILL the test sends and the rerun passes; separately, a pass then a cache hit | the first digest is flaky, both runs recorded; the second is a possible flake with both runs in its history; the cache hit adds no run | record only the answered run; count a probe-proven Farm run as a failure |

### 10.3 End to end

`kbf-it` today starts one server and one daemon on fixed ports with the test-only
local runtime (`kbf-it/m1/run.sh`: the server at 71-76, the daemon at 84-87). This
test needs:

- **two daemons**, on ports picked free per run;
- **a test-only capability** whose probe reads a file the test controls, so a node is
  "made unfit" on Linux CI (the licence check cannot run there), and a test-only
  signature for it;
- **pinned buck2 and Bazel versions**, so the client behaviour of section 3 is
  confirmed for those versions, not assumed from their main branches.

| # | Case | buck2 | Bazel |
|---|---|---|---|
| 25 | One node unfit, one fit | the action passes; `buck2 log show` carries the reroute message | passes; with `--remote_print_execution_messages=all` the reroute message is printed |
| 26 | Both unfit | exit 2 (not 3), `status.message` with the fix shown | exit 34; the fix text in the output; the server ran it at most 3 times in all; Bazel's retries were answered from the memo |
| 27 | A blob removed from the CAS after the front's check | exit 3, the farm-lost message | re-uploads and passes |
| 28 | The planted mutant of test 7 | red | red |

Test 26 is the one that catches a farm fault leaking as an exit code in the clients
people use. Bazel's exit 34 does not by itself tell Farm from Request (section 3, fact
4), so it asserts the text as well.

## 11. Decisions for the project

**Decided** (2026-10-09):

1. OOM: an out-of-memory kill is Farm. The server reruns with a higher memory ask,
   bounded and stopping at the largest node, and remembers the raised ask per action
   key (not per digest) so later runs start there. A kill at the largest node is the
   action's, reported as "needs more memory than any node offers" (6.1). The status
   code, the key's composition, the floor's decay, the over-cap memo, a kill below
   the booking, and the ladder's step, cap and bound are open (12 to 15 and 17
   below).
2. Timeouts: an action's timeout is Action (the client set it), `DEADLINE_EXCEEDED`,
   never rerun. A paused or frozen node is not a timeout: it is a farm error and is
   requeued. Today that holds past G only; between T and G it needs the server reruns
   of item 9. The frozen-daemon gap is #167. No pressure attribution (6.2).
3. Flakes: a digest observed both passing and failing is flaky; every run is recorded
   per digest, farm reruns included, and shown in the API and UI; never rerun until
   green (6.3).
4. #173 is extended before it merges: it reports each Xcode it leaves out, with the
   reason and the fix, in `/v1/nodes`; re-checks periodically and restores an Xcode
   that passes; and alerts, or until `kbf-alert` exists logs at WARN, with an issue
   for alert delivery. In the probe model this is 4.1's licence and `xcrun --find
   clang` probe with its start and periodic triggers, section 7's probe state, and
   section 8's alert in an interim form. What remains for the probe loop afterwards:
   running probes in the action's context (sandbox profile, user, environment), since a
   check from the daemon's own context passes on #172's case; the triggers after a
   farm fault, after a signature match (4.2) and on an operator's request; one probe
   at a time, shared; the 4.1 hysteresis; the Metal, SDK, sandbox-canary and scratch
   probes; withdrawing a capability through a mid-session report change, unless #173's
   re-check already sends one; the withdrawn-everywhere answer and its memo; alert
   delivery.

**Open, with leans:**

5. Adopt the four classes, the Ambiguous rule of 1.1, and the invariant of section 1.
6. Signatures are a reviewed allowlist, every Farm verdict proven by a probe run in the
   action's context, bounded and shared (4.2, 6.4 C).
7. Probes run continuously and withdraw one capability with an alert; recovery is
   automatic, with the 4.1 hysteresis. Re-detection and mid-session report changes come
   first.
8. A capability withdrawn on every node that could serve is answered `INTERNAL` at
   once, not `FAILED_PRECONDITION` after the wait, departing from REAPI's wording;
   I10 is amended to match (4.1).
9. The server reruns Farm and Ambiguous results on other nodes, at most 3 runs per
   operation, then `INTERNAL` with the fix in `status.message`; a 60 s verdict memo,
   cleared on recovery (6.5).
10. Request errors: `FAILED_PRECONDITION` except a malformed action (6.7).
11. Order: keep the reason and node (7); the extended #173; then server reruns (#22)
    with the memory ladder; then the rest of the probe loop and signatures; then
    alerts; then the flake record and memory floors.
12. Past the cap: `FAILED_PRECONDITION` with `ACTION_OUT_OF_MEMORY`, not
    `RESOURCE_EXHAUSTED` or OK exit 137 (6.1, O1).
13. The action key: platform properties, output paths and first argument, plus
    `RequestMetadata` where sent (6.1, O2).
14. Floor decay: halve after 20 passes under half, drop after 30 idle days, numbers
    from pilot data (6.1, O3).
15. An over-cap memo answers a repeat of the digest without a run (6.1, O4); a kill
    below the run's own booking counts against the node where the peak is measured
    (6.1, O5).
16. Node suspicion: a metric first, an alert once pilot data sets a threshold (6.3, O6).
17. The memory ladder: each rung doubles the booking in whole GiB, the cap is the
    largest live or cordoned node that fits the platform, and the ladder is bounded
    apart from the 3-run budget (6.1, O7).
