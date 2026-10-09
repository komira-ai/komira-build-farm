# Failure classes: farm, request, action

When an action fails, the person reading the build log needs one answer first: is this
mine to fix, or the farm's? This document defines the classes of failure, what the
farm does today, and what is **proposed** so that a farm problem is never reported as
the action's own failure. Lease handling is in [daemon.md](daemon.md), outcomes and
retries in [scheduler.md](scheduler.md#outcomes), node reports in
[capabilities.md](capabilities.md), the simulation in [simulation.md](simulation.md).

Line numbers are on `main` at the merge of #177; paths are under `crates/`. "Proposed"
means no code yet.

## 1. The classes

| Class | Meaning | Whose to fix | Example |
|---|---|---|---|
| **Farm** | The farm could not run the action, or ran it on a node unfit to run it. | the operator | Xcode licence not accepted; scratch disk full; lost contact; a blob the CAS lost |
| **Request** | The action cannot run as written, on any node. | the client | a Command with no arguments; an output over the size limit; a blob never uploaded |
| **Action** | The action ran on a fit node and exited non-zero, or ran past its timeout. | the action's author | a compile error; a failed assertion; a crashed test harness |
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
| exits 0 | OK with the second run's result; the first run is recorded as a Farm fault on its node (it counts for node suspicion, 6.3) |
| ends the same way (same signal, not sent by kbf) | **Action**: OK with the second run's result, exit 128+N. The action did it to itself |
| ends any other way (another exit code, another signal) | that run is classified on its own, as if it were the first |
| is a Farm fault | the Farm path (6.5) |

The Ambiguous rerun is one of the operation's 2 extra runs (6.5). An action that kills
itself with SIGKILL every time therefore ends OK with exit 137 after exactly 2 runs; it
is never answered `INTERNAL`.

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
6. A slow or paused node pushing an action past its timeout (arrives as
   `DEADLINE_EXCEEDED`, blamed on the action).
7. Environment drift: a missing tool on `PATH`, the wrong `DEVELOPER_DIR`, leftovers
   of another lease.

### And the other way round

- Program not found (`kbf-driver-native/src/runtime.rs:172`), an output over the size
  limit (`runtime.rs:636`), and a missing input blob at the worker all reach the client
  as the fixed `INTERNAL`. The `MISSING` detail is dropped at `farm.rs:553`, so Bazel is
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
  leaves out an Xcode that fails. It checks once, logs, and raises no alert: actions
  then wait for the unservable bound and are refused, or capacity quietly shrinks. It
  touches `discover`, as #172 does; expect a conflict.
- **#172** fixes the sandbox denials for temp items and the `xcrun` cache. #161
  (orphaned runs) and #174 (retention) change nothing a client sees.
- Merged: **#176** ends open Execute streams `UNAVAILABLE` on shutdown; **#177** names
  the worker on OK results only, and logs each requeue with its reason.

None adds retries, keeps the daemon's reason, alerts, or classifies after a run.

## 3. What the clients do with each answer

Read from the source of buck2 (`main`), Bazel (`master`) and the REAPI proto on the
day this was written; not yet confirmed against pinned releases (test 13 pins them).
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
  over-booking). Any other SIGKILL or SIGTERM is Ambiguous (section 1.1). A signal the
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
| Farm, rerun succeeded | OK | the action's | `ExecuteResponse.message`: "ran on B after a farm fault on A: <reason>". Visible as for Action | as Action |
| Farm, runs used up, or the capability withdrawn everywhere | `INTERNAL` | none | `status.message`: "kbf farm fault on <node>: <reason>. Operator fix: <fix>. Runs: n." Both clients show it | no |
| Request | `FAILED_PRECONDITION`, or `INVALID_ARGUMENT` for a malformed action (6.7) | none | `status.message`: the limit or field at fault | no |
| Missing blob | `FAILED_PRECONDITION` + `MISSING`, nothing else | none | `status.message`: the blobs | no |
| Server shutting down | `UNAVAILABLE` (#176) | none | `status.message`, as today | no |

**Details.** A non-OK answer other than a `MISSING` one also carries a
`google.rpc.ErrorInfo` with domain `kbf` (the front already uses it for
`NO_WORKER_CAN_RUN`, `kbf-front/src/execution.rs:92`), reason `FARM_FAULT`,
`REQUEST_ERROR` or `ACTION_TIMEOUT`, and metadata `node`, `signature`, `probe`, `runs`.
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
- Over-booking after its rerun is `FAILED_PRECONDITION`, not `RESOURCE_EXHAUSTED` (6.1):
  the spec's quota wording fits the latter, but Bazel would retry it five times at the
  same size.
- Program not found inside the input root is answered as an `ActionResult` with exit
  127 that the program never produced (6.7). It is safe only because a non-zero exit is
  never cached.

## 6. Open decisions, with options

### 6.1 Out-of-memory attribution

- **A. Booking decides.** Used more than the lease booked: the action's. Killed below
  its booking: Farm (the node overcommitted).
- **B. Always Farm, rerun with a larger booking** (double, up to the largest node);
  only past the largest node is it the action's.
- **C. Always the action's** (today's native behaviour, without the retry).

A blames correctly but needs the cgroup count on every exit and, on macOS, a way to
see a memory-pressure kill (UNVERIFIED that `kern.memorystatus` or the unified log
gives it cheaply). B is simple and self-healing but hides a real leak behind reruns. C
is wrong for node pressure. **Lean: A with B's retry.** Over-booking is rerun once with
twice the booking, whoever set the booking. If it fails again, answer
`FAILED_PRECONDITION` "used X, booked Y". Who set the booking changes the text and the
accounting only: when the estimator set it, the message says so and both runs are
charged to the farm. Killed below its booking: Farm. A SIGKILL kbf did not send and
cannot attribute: Ambiguous.

### 6.2 Timeouts on slow or paused nodes

- **A. Always the action's** (today).
- **B. Measure the node.** The daemon records, per lease, time the machine was
  suspended (monotonic clock against boot clock on Linux, continuous against absolute
  time on macOS), time the daemon or child was stopped, and node pressure (Linux PSI
  `full` on CPU, memory, IO). Above a threshold the timeout is Farm and reruns
  elsewhere with the same timeout.
- **C. Rerun every timeout once** elsewhere.

C doubles the cost of every real hang. A blames the action for a sleeping node. **Lean:
B**, with thresholds set from pilot data, and F2.6 (suspend and resume) extended to
expect a Farm verdict above them. #45 makes an Action timeout show its partial output
in Bazel; buck2 drops the result on any non-OK status, so buck2 users never see it.

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
  question (#22).
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
`FAILED_PRECONDITION` for output over the size limit and over-booking. Program not
found: a path inside the input root is Action (exit 127 with kbf's message on stderr,
as a shell and podman would; a spec departure, section 5); an absolute path outside it,
missing on this node while peers of the same platform have it, is environment drift,
so Farm.

## 7. Labelling: where the class is recorded

- **Client**: section 5's status, message and `ErrorInfo`; `ExecutedActionMetadata.worker`
  on every result, failed ones included (in the partial result where there is one).
  Neither client shows `worker` today, so the node also goes in the text.
- **Server log**: one line per run with operation, lease, node, class, reason,
  signature and fix. Today it has the code only (`farm.rs:553`).
- **Daemon log**: the reason and class beside `lease finished` (`lease.rs:119`).
- **`server_logs`**: a run history blob per operation (each run's node, class, reason,
  stderr tail). Bazel fetches it on failure; the UI reads it.
- **`/v1/nodes`**: per node, each capability's probe state (ready, withdrawn with
  reason and fix, since when) and the last farm faults.
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
| Farm fault without a probe | a driver or control fault | node, reason, count in the last 15 minutes | 15 minutes pass with no such fault on that node |
| Node suspicion | section 6.3's ratio over a threshold | node, digests that passed elsewhere | the ratio drops below the threshold |
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
- time from withdrawal to recovery
- **the leak canary**: OK results with exit 69, 126 or 127, or ended by a signal kbf
  did not send and the kernel did not raise for the program's own fault, per node,
  compared with the node's peers on the same platform. Ordinary crashes (SIGABRT,
  SIGSEGV) are left out. A node far above its peers has a farm fault with no
  signature yet.

## 10. Test plan

Each test says what it proves and names the planted mutant that must turn it red.

### 10.1 Simulation (`kbf-sim`)

New invariants beside [simulation.md](simulation.md#4-invariants) I1-I15:

- **I16. An OK answer comes only from a fit run.** An OK answer is produced only by a
  lease whose run the daemon model marked as on a node fit for the action's
  capabilities.
- **I17. Farm runs are bounded and spread.** No operation runs more than 3 times after
  Farm or Ambiguous results, and no two of those runs are on the same node.
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

### 10.2 Real processes

Daemon and native or container driver, with fakes like #173's fake `xcodebuild`, and
the daemon's clock seam driven by the test.

| # | Setup | Expect | Mutant |
|---|---|---|---|
| 9 | Licence lapses after start: the fake answers `-license check` with 0, then 69; the action exits 69 with the licence text | the probe reruns, the Xcode is withdrawn, the Result is Farm with the exact fix string pinned; flip the fake back, trigger 2 probes 60 s apart on the test clock: the Xcode returns | trust the signature without the probe; check only at start (#173 as written fails this) |
| 10 | The action prints the licence text while the licence is fine | OK, exit 69, no withdrawal | drop the proving probe |
| 11 | The licence text placed in stderr just past the last 64 KiB, and just inside it | past: Action; inside: probe runs | search all of stderr, or none of it |
| 12 | A sandbox profile that denies the `xcrun` cache; the action prints `unable to find utility` | Farm (the probe, run in the sandbox, fails too) | run the probe outside the sandbox |
| 13 | A probe that never answers | after 60 s on the test clock, the capability is withdrawn with "probe timed out" | no bound on the probe |
| 14 | Three leases match the same signature at once | one probe runs; all three get its verdict | a probe per lease |
| 15 | The action blocks on a fifo; the test sends SIGKILL to it while it is blocked | Ambiguous, rerun | forget which kills are ours |
| 16 | kbf's own timeout kill | `DEADLINE_EXCEEDED`, no rerun | as 15 |
| 17 | The action calls `exit(137)` | Action, OK exit 137, no rerun | classify on the exit value instead of "ended by a signal" |
| 18 | The action sends itself SIGKILL every time, 2 nodes | OK, exit 137, after exactly 2 runs | count the Ambiguous rerun outside the budget; answer `INTERNAL` |
| 19 | Container child OOM, parent exits 1: client booking 64 MiB, child allocates 128 MiB | over-booking: one rerun at 128 MiB; the child then allocates 256 MiB, so `FAILED_PRECONDITION` "used X, booked Y"; never OK exit 1 | check `oom_kill` on 137 only |
| 20 | Scratch full during setup (a small filesystem as scratch); full during the run | setup: Farm with the scratch probe failing; run: signature matched and proven | treat setup errors as Action |
| 21 | Every `RuntimeError` variant and every server-side failure, table-driven; the mapping is a `match` with no wildcard arm, so a new variant does not compile until it is classified | the class and code of section 5 | `Failed` mapped to `INVALID_ARGUMENT`; `MissingBlob` mapped to `INTERNAL` (today's defect) |
| 22 | A `MISSING` answer, from the front and from the server's re-check | exactly one detail, the `PreconditionFailure` | add an ErrorInfo to it |
| 23 | Missing blob: delete it from the CAS after the front's check; separately, break only the worker's fetch | `MISSING` to the client; a rerun elsewhere | the server answers `INTERNAL` for both |
| 24 | Alerts: 5 faults on one node and capability; the probe passes twice; the probe flaps 4 times in an hour | 1 alert with count 5; resolved; one reopened alert, notified once | key alerts per fault; no flap rule |

### 10.3 End to end

`kbf-it` today starts one server and one daemon on fixed ports with the test-only
local runtime (`kbf-it/m1/run.sh:73-84`). This test needs:

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

1. Adopt the four classes, the Ambiguous rule of 1.1, and the invariant of section 1.
2. Signatures are a reviewed allowlist, every Farm verdict proven by a probe run in the
   action's context, bounded and shared (4.2, 6.4 C).
3. Probes run continuously and withdraw one capability with an alert; recovery is
   automatic, with the 4.1 hysteresis. Re-detection and mid-session report changes come
   first.
4. A capability withdrawn on every node that could serve is answered `INTERNAL` at
   once, not `FAILED_PRECONDITION` after the wait, departing from REAPI's wording;
   I10 is amended to match (4.1).
5. The server reruns Farm and Ambiguous results on other nodes, at most 3 runs per
   operation, then `INTERNAL` with the fix in `status.message`; a 60 s verdict memo,
   cleared on recovery (6.5).
6. OOM: booking decides, one rerun at double the booking (6.1, A with B's retry).
7. Timeouts: measure suspension and pressure; a node-caused timeout is Farm (6.2 B).
8. Flakes: record and alert on node suspicion; never rerun Action failures (6.3 B).
9. Request errors: `FAILED_PRECONDITION` except a malformed action (6.7).
10. Should #173 merge as is (start-only, log-only) and be extended, or be reworked to
    the probe model first? Lean: merge it, since it fixes the pilot's failure today,
    with its description saying test 9 is expected to fail until the probe loop lands;
    build the probe loop and alert next.
11. Order: keep the reason and node (7); then server reruns (#22); then probes and
    signatures; then alerts; then timeout and OOM attribution.
