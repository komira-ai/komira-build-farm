# CI/CD for kbf: beta, gamma, prod, and the walk back

Status: design. Nothing in this page is built except where it says **[F]** (read in the tree). **[P]** is a proposal.
**[V]** is something to verify by measurement before relying on it. Open pull requests and issues are named by number;
re-check their state before acting on this page, because the table in section 12 rots first.

## 0. The plan in one page

Every merge runs a **beta**: GitHub Actions stands up a whole farm on loopback (a frontend, three storage-and-daemon nodes,
one plain daemon, a pinned Envoy in front), runs real Buck2 builds, then updates every role while a build is running and
kills a node while a build is running. The assertions are numbers, not logs: zero failed actions, zero lost blobs, and
under-replicated blobs back to zero.

A commit that passes beta may be offered to the operator's own farm: **gamma**. The farm's pull updater applies it. The
deploy is itself a test (no build failed because a connection was dropped), and then a 24 hour **bake** runs with the
operator's real usage as the canary. The bake ends in a verdict document.

A commit with a passing bake can be published: **prod** is the existing attested-artifact release (tag, files, notes), gated
in GitHub on the verdict. **Rollback** is the updater walking back through the same mechanism it walked forward with: the
desired commit is set to the older release under a higher generation.

```
 merge ──> artifacts (build once, attest) ──> beta (Actions, ephemeral farm)
                                                  │ result attested (same set hash)
                                                  v
                          gamma: operator's farm, pull updater, deploy-as-test
                                                  │ 24 h bake, real usage
                                                  v
                                 verdict ──> prod: tag, attested files, notes
          any gate red ──> rollback: desired = the old commit, higher generation
```

Four rules hold in every stage, and each is a rule that exists in the project already: **one frontend update process
everywhere** (a new instance on other ports behind Envoy, the old one drains, in-flight `Execute` streams are kept; the
Buck2 client never re-sends a broken `Execute`; the serving frontend is never restarted), **nodes are updated in any order
when idle or drained, within N and N-1**, and on an upgrade the frontend goes first, only through Envoy, never waiting for
`open_executions == 0` (the order of `split-mode-updates.md`); **the farm pulls**: no stage pushes to a host and no stage
holds a way into the operator's network; and **hosts trust the build, not the operator**: a host verifies the GitHub
artifact attestation of what it installs, and there is no operator signing for now.

## 1. Goals and non-goals

Goals:

1. A regression that fails a build during an update is found on the merge that introduced it, not on the operator's farm.
2. The operator's farm is never the first place a release runs, and is never updated by a person typing commands to a host.
3. The bytes that run in the full beta, in gamma and in prod are the same bytes: one build per commit, promoted, never
   rebuilt. (The merge-queue smoke runs the queue ref's own build, which is not what ships; section 4.1. The first release
   is rebuilt after the branch merges to main and re-runs beta and gamma on that commit; section 13, Q1.)
4. Every gate is a number with a threshold that the doc states, and every gate fails closed: an absent metric is a failure.
5. Rollback is a normal, tested path, and the doc says which releases cannot be rolled back.

Non-goals: a deployment tool for anything but kbf; multi-region or multi-farm rollout; push-style deployment of any kind;
making the farm reachable from a public runner.

## 2. The stage model, reused from kci, and where kbf differs

kci is the release tool in the sibling komira repository (its stage model is in that repository's `docs/ci.md`). Restated
here so this page stands alone: a release is a list of stages, each with a name, an `after` ordering, a named GitHub
environment and a result document; one job per stage; the same release directory is promoted to every stage and never rebuilt
(`build.set_hash == gamma.set_hash == prod.set_hash`); a validation job holds no identity token; `prod` needs a reviewer and
is manual. kci's stages are build, gamma and prod, and kci's gamma is a publish channel. Here, "reuse" means vocabulary and
shape only: kbf's build stage is `artifacts.yml`, its gamma is a pull-based farm with no Actions job, and beta is added.

What carries over, unchanged in meaning:

| kci | kbf |
|---|---|
| stage list with `after:` | `build` (implicit, `artifacts.yml`), `beta`, `gamma`, `prod`, in a small checked-in stage file |
| one environment per stage | `beta` (no secrets, no reviewer), `prod` (reviewer); `gamma` is described below |
| build once, promote by set hash | `artifacts.yml` already builds once per commit and writes `SHA256SUMS`; the set hash is the digest of that file |
| validation never a skip, distinct exit code | each stage ends in a result document with a verdict; "not run" is not "passed" |
| result document written RUNNING first, FINISHED on every exit | the same, one JSON per stage, uploaded as a workflow artifact |
| workflow checked against the stage list | a lint in `crates/kbf-it/tests` beside the existing workflow lints (hosted runners only, SHA-pinned actions, `permissions: {}`) |
| least privilege: the job with the publish token holds nothing else | `attest` is already the only job with `id-token: write`; the new jobs hold `contents: read` |
| dry run by default | `release.yml` dispatch is already a dry run; gamma-offer and prod get the same switch |

Where kbf differs, and why:

1. **There is a beta stage; kci has none.** Beta is new: an ephemeral farm per merge that
   publishes nothing. It is closest to kci's build-plus-validation, not to a channel.
2. **Gamma is not a publish destination.** kci's channels are places bytes are pushed, permanent and never overwritten.
   Gamma is a farm that pulls: "the desired release" is the `update/desired {generation, commit, set_at}` row in the
   operator's frontend state store, and it moves. A moving pointer is the opposite of never-overwrite, so every move bumps a
   generation that only grows, and a rollback is the older commit under a higher generation. Downgrades are allowed; the
   generation is the guard (`pull-updates.md`, `split-mode-updates.md` 2.3).
3. **Gamma and its bake run outside Actions.** A hosted runner on a public repository must not reach the operator's farm, so
   the farm's metrics cannot be read from a workflow. kci's farm-connected jobs are not adopted. The operator side runs the
   evaluator and returns a verdict (section 7).
4. **There is a time gate.** kci has no bake, soak or metric gate, and no rollback verb. All three are new here.
5. **Code is not shared.** kci is Mojo on Buck2; kbf is Rust with `xtask`. The stage file is plain data checked by a Rust lint.
6. **kci's DEPLOY step** (a cloud resource graph apply) does not describe restarting kbf processes on a host. The
   deploy here is the pull updater and the frontend-ordered node steps of `split-mode-updates.md`.

### 2.1 The stage file [P]

`release/stages.toml` (name is open), read by the lint and by a small `xtask` that the workflows call:

```toml
[[stage]]
name = "build"
runs = "actions"          # artifacts.yml, the implicit build stage; attests on push only
after = []
result = "SHA256SUMS"

[[stage]]
name = "beta"
runs = "actions"          # a job in .github/workflows/beta.yml
environment = "beta"
after = ["build"]         # smoke: the merge_group build; full: the push build and its attestations
result = "beta-result.json"

[[stage]]
name = "gamma"
runs = "operator"         # not an Actions job: offer, deploy, bake, verdict
after = ["beta"]
result = "gamma-verdict.json"

[[stage]]
name = "prod"
runs = "actions"          # release.yml
environment = "prod"
after = ["gamma"]
result = "release.json"
```

The lint refuses a workflow job that names a stage not in the file, an `actions` stage with no job, a `pull_request` trigger
on any of these workflows, an unpinned `uses:`, and `id-token: write` anywhere but `attest`. `runs = "operator"` stages have
no job; the lint instead checks that `release.yml` verifies the verdict (section 8).

## 3. The artifact every stage consumes

`artifacts.yml` is the build stage and is not changed in kind. **[F]** It builds `kbf`, `kbf-daemon`, `kbf-server` and
`kbf-deploy` for linux x86_64, linux arm64 and darwin arm64, deterministic tarballs, SBOMs, `SHA256SUMS`; on a push to main
or feature/arch-v2 `attest` adds provenance and SBOM attestations and `verify` checks them. An attestation names its ref,
so a feature/arch-v2 asset does not verify as main's.

What the stages add:

- **The full beta runs the attested files**, not a fresh `cargo build`. It runs after the push build has attested (a job
  of `artifacts.yml` with `needs: verify`, section 4.1), downloads that output, runs `cargo xtask verify-assets` on it, and
  starts the farm from those binaries. Otherwise beta would test bytes nobody ships. The harness (`cargo test`) is the one
  thing built in the beta job; it is test code, not a shipped file.
- **The merge-queue smoke is unattested.** `artifacts.yml` also runs on `merge_group` (and `pull_request`) but attests only on
  a push, so no attested files exist before the merge. Smoke runs the binaries that `merge_group` build produced (same recipe,
  unattested, not the shipped bytes), or, if waiting for that build is too slow, binaries it builds itself with `cargo`.
- **The beta result is attested.** A last job, the only one in beta with `id-token: write` and `attestations: write`, attests
  `beta-result.json` (commit, set hash, harness commit, every gate with its measured value). No repository code runs in that
  job, as in `attest` today.
- **The offer** (section 6) sets the desired commit, and the hosts fetch the release and verify its artifact attestation. A
  node never sees a beta result; `kbf release offer` checks it before it sets anything (section 5.1).
- **A previous release for N-1.** Beta also fetches the last released set (or, before there is one, the newest earlier
  commit with an attested build on the same branch) and verifies it the same way. This is how the N/N-1 jobs of section 9 have
  an N-1.

Releases are cut from main only (decided, #961). Development continues on feature/arch-v2 until the farm works; the branch
then merges to main (#753), and from then on every release is cut from main. Beta and gamma run on the feature branch
meanwhile; nothing is published from it.

## 4. Stage 1: beta

### 4.1 Trigger and cost

Two layers, because "on every merge" and "a merge must not wait an hour" pull apart:

- **Smoke, in the merge queue** (`merge_group`): start the whole farm, run builds, assert zero failures. No update, no kill.
  It needs a build of the queue ref, so its duration is that build plus the smoke, not the smoke alone **[V]**: the target of
  10 minutes holds only if the build is cached or the smoke uses its own `cargo` binaries (a cheaper build than the multi-platform
  one, since smoke needs one platform). Measure that in B-0 before the check is made required; the order of preference is the
  `merge_group` build's output, then smoke's own build. Smoke is unattested and says so in its result document.
  `merge_group` runs repository code from the queue ref with no fork code, so the root-level helpers are acceptable there.
- **Full, after the merge** (`push` to feature/arch-v2 and main): rolling update of every role under a running build,
  kill-a-node, the Envoy swap, the all-in-one variant, N/N-1, on the attested bytes. It is a job of `artifacts.yml`
  (`needs: verify`, on `push` only) so the order after attestation needs no `workflow_run` (which runs from the default
  branch's workflow file and carries its own trust caveats). A red full run does not revert anything; it **blocks the
  commit from being offered to gamma** (the offer needs its beta-result attestation) and opens an issue automatically.
  A scenario becomes a required part of the full run on the day its blocking pull requests merge (section 4.5), not before,
  because a fail-closed gate on an absent metric is red by construction.

Neither runs on `pull_request`: they start a farm with root-level helpers and attest results, and a pull request from a
fork must not run either. The lint of section 2.1 enforces the absence.

A newer push cancels an older push's run, as `artifacts.yml` already does; a cancelled beta is "not run", never "passed".
The offer takes the newest commit with a green beta, so skipped commits cost nothing.

### 4.2 Runner shape: does a whole farm fit on a hosted runner

**[V]** The proposal is the standard hosted Linux runner (about 4 vCPU and 16 GB on a public repository; the 6 hour job
limit is far above need). The rehearsal harness already starts this topology as a `cargo test` in `test-x86`. **[F]** A
rough budget, to be measured before it is relied on:

| process | role | expected footprint |
|---|---|---|
| frontend (two instances during a swap) | scheduler, state store, Envoy upstream | a few hundred MB each |
| 3 storage-and-daemon nodes | storage, plus a worker each | the largest; segment size and cache caps set small by the harness |
| 1 plain daemon | worker | small |
| Envoy | pinned version | about 100 MB |
| Buck2 client, plus the actions | the load | the variable part |

The rule that keeps it inside the box: the harness sets segment size, cache and tier limits to test values, and the load is
actions that sleep or touch small files (the `//:pause` shape that the `m1`/`m2` cells use), plus a modest compile.
**Beta is a test of the update and failure paths, not a benchmark.** If a measurement shows the farm does not fit, the order
of remedies is: smaller limits; then fewer concurrent actions; then a larger GitHub-hosted runner (billed, unlike the standard
one; still ephemeral and managed by GitHub); and last, and only with the safety conditions below, a self-hosted runner.

Why self-hosted is the last resort, on a public repository: a self-hosted runner executes whatever a workflow tells it, and
a workflow can be changed by a pull request. A non-ephemeral runner keeps state between jobs, so one job can leave something
for the next. The conditions if it is ever used: the runner is ephemeral (one job, then the machine is destroyed), it sits in a
network with no route to anything of the operator's, it takes only the `beta` workflow, that workflow never triggers on
`pull_request` or on a fork's code, the job holds no secrets, and the runner group is restricted to the one repository and
workflow. Arm runners are hosted (`ubuntu-24.04-arm`) and available for an arm beta variant; darwin is covered by `native-macos`
and `sign-darwin` and has no farm role in beta.

Cost shape **[V]**: per merge, one smoke (minutes) in the queue and one full run after; standard hosted runners on a public
repository carry no per-minute charge (a larger runner does). The default build concurrency in smoke is a figure measured in
B-0, not the 20 of T5, which is a likely memory risk. The cost that matters is wall time and queue depth, which is why full is off the
merge path. Record job duration in the result document so a creeping beta shows in the numbers.

**Container runtime [V].** The project runs containers with crun only. The harness today uses a stand-in runtime over a planted cgroup tree.
Real crun needs cgroup v2 delegation and a systemd scope; the podman jobs in `ci.yml` arrange that with a `Delegate=yes`
unit, and the same approach is to be tried for crun on `ubuntu-24.04`. Until that is shown to work (#936), **B-0 uses the
stand-in**, and the page says what that weakens: the `//:pause` and compile loads run through no real container path, so
beta proves the update and failure paths and not container execution (covered by the existing real-runtime jobs). The
runtime swap is a one-line change of the supervisor's configuration once #936 lands, not a rewrite.

### 4.3 The harness

`crates/kbf-cli/tests/rehearsal_in_a_box.rs` with `rehearsal/{pki,procs,units}.rs` **[F]** starts a real frontend, three
storage-and-daemon nodes and one plain daemon, each a real `kbf` process whose argv is the `ExecStart=` line that
`kbf_deploy::linux::modes::render` writes, each with its own loopback address and its own certificates from the farm's
certificate rows. A Buck2-shaped client writes and reads the CAS, runs one `Execute` and gets an action-cache hit. It
updates nothing, kills nothing, runs no real Buck2 and has no Envoy. **[F]** Its stated deviations: a directory as the cold
store, a stand-in container runtime over a planted cgroup tree (#936 removes this deviation: crun only), and no systemd.

The proposal is to grow it into a library (`rehearsal`) that two test cells share, instead of a second harness:

- `procs`: a supervisor with `start(role)`, `term(role)`, `kill(role)` (SIGKILL), `restart(role)`, and logs per process kept as
  artifacts. No role is started any way but the rendered unit line, so the unit text stays under test.
- **Envoy**: the pinned version (the one `test-x86` already fetches by checksum) started from the `box` topology render
  (#925), with the frontend as instances `a` and `b`. The Buck2 client goes through Envoy, never straight to a frontend.
- **Cold store**: a directory by default (fast, no service); the `file://`-versus-MinIO difference is covered by the
  existing `objstore-s3` job and is not a beta concern.
- **Load**: the pinned Buck2 (the `kbf-cell m2` machinery **[F]**) runs builds in a loop for the whole scenario. Three
  kinds of action: a short compile; a long action (30 s, `//:pause` style) so builds span an update; and an uploader that
  writes new blobs so write paths are live during the roll. Concurrency is a harness parameter (default 20 builds, as T5).
- **Measurements**: a scraper reads every process's metrics endpoint every second into the result document; the builds' event
  logs give the per-action outcome.
- **The update under test is the real update code**, not a script that restarts processes: the frontend handover command,
  the node step and the pull-updater client and root helper, in that order of availability (milestones B2 to B4 below). The
  one honest deviation is that the supervisor, not systemd, starts the processes; the `systemctl` edge is covered by gamma.
  The root helper runs under `sudo` on the runner (`sudo-test` already does this in `ci.yml` **[F]**).

### 4.4 Scenarios and gates

All scenarios run inside one farm start where they can, to save minutes; each is a separate named test so a red run says
which. Each scenario states the defect it exists to catch, and the full run is checked once with a planted mutant per
scenario (the mutant is listed; a gate never seen red is not a gate).

| # | Scenario | Gate (all must hold) | Defect it catches (mutant) |
|---|---|---|---|
| B0 | `farm_under_build_load` | the load runs 5 minutes with no update: 0 failed builds, 0 failed actions, `kbf_cache_lost_blobs == 0` | a farm that cannot hold the load it is meant to be updated under (shrink a node's cap) |
| B1 | `roll_storage_nodes` | all three storage-and-daemon nodes updated one at a time, idle by idle (`split-mode-updates.md` T1) | two nodes at once (drop the slot CAS); a write failing in the planned-down window (skip planned-down) |
| B2 | `roll_frontend_envoy` | a handover through Envoy with 20 builds in flight and one `Result` held at the stand-down (T5, T6, E1, E2) | a fence after demote; no retry policy; GOAWAY with no health flip |
| B3 | `roll_daemon` | the plain daemon updated at zero leases; a leased daemon is deferred (T2, T15) | picking on lease count alone |
| B4 | `roll_everything` | B1 to B3 in the production order (nodes, frontend, daemon hosts) inside one running build loop | an ordering that breaks N/N-1 |
| B5 | `kill_a_storage_node` | `SIGKILL` of one storage-and-daemon node mid-build, restart it | the loss of a blob owned once; a write with no failover (depends on #939) |
| B6 | `kill_a_daemon` | `SIGKILL` of the plain daemon with a lease in flight | a Farm-class failure reaching the client as an action exit code (`failure-classes.md`) |
| B7 | `kill_frontend_cold` | the crash matrix, wired as a required job (#900) | each case's own defect |
| B8 | `roll_all_in_one` | the all-in-one process updated by the same handover (`aio-handover.md`) | a second update path for the small topology |
| B9 | `mixed_versions` | N and N-1 in both directions, with and without a build in flight (section 9) | a made-required field; a repurposed field |
| B10 | `rollback_walks_back` | after B4, set the desired commit to N-1 under a higher generation and walk back (T16) | rollback restarting the frontend |
| B11 | `rollback_mid_roll` | a rollback (desired set to the older commit) issued (a) after the first node step and (b) after the frontend handover, from that half-rolled state, with a build in flight; N and N-1 mixed in the reverse order | a walk back that assumes a clean farm; a node mid-step abandoned instead of finished or reverted |

The gate expressions common to B1 to B6, B8 and B10, read from the scraped metrics and the builds' event logs:

- **0 failed actions.** Every action of every build in the load ended with its own exit code, none with a farm-class
  failure. The measure: the builds' event logs, and `kbf_execute_errors_total` delta equal to the number of actions the
  scenario failed on purpose (zero here). Today that counter has no cause label, so a dropped connection cannot be
  told from a client abort except through `kbf_frontend_relay_lost_total`. **[F]** A failed-actions-by-class counter is a
  missing piece (section 12, M10); until it exists the gate is: event logs show zero failed actions **and**
  `kbf_execute_errors_total` delta is zero.
- **0 builds lost to a handover**: `kbf_frontend_relay_lost_total` delta is 0.
- **0 lost blobs**: `kbf_cache_lost_blobs` is 0 at every scrape, and the delta of `kbf_cache_blob_losses_total` (a counter
  that never falls) is 0 over the scenario. The gauge alone can fall back to 0 after a loss.
- **Under-replicated blobs back to 0**: `under_replicated_blobs` is 0 within the settle window after each node step
  (**[P]** 300 s in beta; tuned from B1 measurements) and at the end. It may be above 0 only between a node going out and
  the settle window ending, and never for two nodes' worth at once. The gate is **absent means failed**: if the metric is
  not exported the run is red (T12's "absent debt metric" rule). The metric is not on feature/arch-v2 yet (#912, #913).
- **Never two members non-Active at a sampled instant**, in B1 and B4. **[missing]** There is no membership-state gauge
  (M13). Until it exists the gate reads the node log's state transitions and the supervisor's own record.
- **Each updated process restarted once, none other**: **[missing]** no process boot counter exists in the tree (the only
  boot id is the kernel's, in `kbf-driver-native`; M13). Until it exists: in beta, the supervisor's process table (pid and
  start time per role); in gamma, the updater's phase log.

B5 and B6 are expected to *pass the build*. They are not "the farm survives anything": a second unplanned loss while one
node is out is the stop rule of T14 and is tested there, not asserted green here.

### 4.5 Order of work: the beta is built up in steps

A beta that needs every update PR to land first would not start for a long time. The harness is delivered as milestones, each
useful alone and each required from the day its blocking pull requests land (B-2's gates are red by construction until
#912 and #913 export their metric, so B-2 is not enabled before them):

| milestone | adds | blocked on |
|---|---|---|
| B-0 | harness library, the pinned Envoy, the real Buck2 load, B0, smoke in the merge queue (on the stand-in runtime, section 4.2) | #925 box render; the runner measurement (M21) before smoke is required |
| B-1 | B5, B6, B7 (kill-a-node, kill-a-daemon, crash matrix required) | #900, #939 (write failover) |
| B-2 | B1, B3 (node and daemon roll) | #912, #913, #914, #917 (the metrics and the picker) |
| B-3 | B2, B8 (frontend swap through Envoy, all-in-one) | #930, #937, #938, the wiring PR of #901, #925, #934 |
| B-4 | the pull updater replaces the stand-in for the real thing in B1 to B4; B10 and B11 (rollback) | #908, #929, the poller of `split-mode-updates.md` 6.2 |
| B-5 | B9 against a real N-1, state-store and wire checks | a released N-1 (or the fallback of section 3) |

The assertions "0 failed actions" are only reachable once #791 (the frontend fails every in-flight action on restart) is
closed and the upstream behaviour in #792 is understood (this binds gamma's deploy-as-test too, section 5.2); B2 exists to prove that, and if it cannot be made reliable the model
is wrong and that is reported, as `split-mode-updates.md` says of T5, T6 and E1.

## 5. Stage 2: gamma, the operator's own farm

Gamma is the operator running the release on the farm they use. This page says "the operator's farm" and writes no address,
host or account, so the same text is right for anyone who runs kbf and chooses to take the same role.

### 5.1 The offer

A commit becomes offerable when `kbf release offer` ([P], run on the operator's side) has checked, from outside the farm:

1. the artifacts' provenance and SBOM attestations for the commit and set hash (`verify-assets attestations`);
2. the beta-result attestation for the same commit and set hash, with every gate green and its measured values inside the
   thresholds of section 4.4;
3. that every platform the farm runs is complete for the commit in the frontend's release store (tarball and attestation).

On a red or missing beta result, or an incomplete release, the command refuses and changes nothing. Otherwise it promotes the
commit by setting `update/desired` (`POST /v1/update/desired {commit}`, compare-and-set; the generation moves only when the
commit differs). It signs nothing. Each host's root helper verifies the GitHub build attestation offline when it stages, and
the frontend does not have to **[F in the design, unbuilt]** (`pull-updates.md` section 2; #908 node client, #929 root helper).
Hosts trust the attestation of the build, not the operator. The only exception is the held installer of a test farm, which
runs with attestation verification off; that is a test-farm setting, not a default and not part of this contract.

Offering is dry-run by default (`--plan`): it prints what would change and which gates it read.

### 5.2 The deploy is a test

The pull updater, the node steps and the frontend handover in the order of `split-mode-updates.md` run on the real farm
with real builds in flight. The deploy is a test because it is checked as one:

- **No build failed from a dropped connection**: `kbf_frontend_relay_lost_total` delta is 0 over the deploy, and every
  build that was in flight at any point of the deploy (the evaluator records the invocation ids at deploy start and at every
  step) finished without a farm-class failure (`failure-classes.md`: a farm problem never reaches the client as an action
  exit code). The invocations record carries the failure class **[F: `crates/kbf-invocation`]**.
- **No lost blobs** over the deploy: `kbf_cache_lost_blobs == 0` and the `kbf_cache_blob_losses_total` delta is 0.
- **Under-replicated back to 0** after each node step within the settle window; the run's own gates (preflight, one storage
  node out at a time, repaired before the next) hold, and every one fails closed.
- **Commit-confirm held**: no process hit its local auto-revert (a node that never becomes active reverts itself in
  minutes; that revert firing during a deploy fails the deploy).

The deploy-as-test gates depend on #791 and #792 (a frontend restart fails in-flight actions, and the upstream client does not
re-attach), exactly as B2 does (section 4.5). Until both are closed the zero-failure gates cannot pass on a real farm, and
gamma deploy-as-test is blocked, not waived.

A deploy that fails any of these halts, walks back what it has changed (section 8) and fails gamma. Nothing about a halt is
silent: the result document names the gate, the value and the host step. The walk back is itself a frontend handover and node
steps and can drop connections too. It is held to the same checks and is reported, but a failure during it is **a red
verdict and a page, not a retry loop**: nothing retries a walk back on its own, and the host's commit-confirm revert is the
floor.

### 5.3 What gamma needs that beta cannot give

Real usage (builds nobody scripted), real devices and hosts, systemd, the real cold store, real load shape and duration.
Beta covers the update logic; gamma covers the environment. A defect that only the operator's environment shows becomes a
beta scenario in the same change that fixes it (section 11).

## 6. Promotion: the frontend's desired state

Gamma and prod are not channels and there is no feed. The pull updater reads the state the operator's frontend already
keeps (`pull-updates.md` is the authority for the rows; `split-mode-updates.md` for the order of the rollout).

- **One set of rows in the frontend's state store**: the release rows (`update/release/<commit>/<platform>/<name>`);
  `update/desired {generation, commit, set_at}`; `update/slot` (the rollout slot); `update/host/<id>` (per-host progress); and
  `update/policy`. The disk release index is the source of truth for the bytes; the rows say what is wanted and how far the
  rollout has got.
- **Promotion is setting the desired commit.** Gamma is promoted by setting the desired commit (and so a new generation) on
  the operator's frontend, after beta passed and the offer checks of 5.1 passed. Nothing else in this page moves a farm.
- **The generation is monotonic and never reused.** It is the guard: hosts act on a generation higher than any they have
  acted on, so a replayed older order is refused. Downgrades are allowed; there is no floor on commits. A rollback is the
  older commit under a higher generation (section 8.2).
- **Rollout order is split-mode's.** Nodes update in any order when idle or drained. On an upgrade the frontend goes first,
  and only through Envoy; across a protocol difference on a downgrade the nodes go first. The updater never waits for
  `open_executions == 0` and never restarts the serving frontend.
- **Which release a farm follows is that farm's own desired state.** The operator's farm is promoted by `offer` after beta.
  A farm that runs the public release has its operator set the desired commit to the published one (by hand, or by its
  `update/policy`); no stage reaches into it.
- Nothing here needs a server that can reach a farm: the frontend polls GitHub for new attested releases and only ever
  proposes them (`pull-updates.md`); the operator or the offer command decides.

### 6.1 What the offer checks, and who runs it

`kbf release offer` ([P]) is run by the operator or by the operator's own scheduled job on the operator's side; it is never
run by a public workflow, because that would put a way into the farm in Actions. It refuses on any failed check of 5.1 and
sets nothing. It holds no signing key and no key is needed: the host-side trust is the GitHub attestation of the build.
Signing operator-authored manifests for hosts to verify is deliberately not done for now; if it returns it is a new decision
with its own page.

## 7. The 24 hour bake

After a clean deploy the farm runs the release for 24 hours on the operator's real usage. The bake is run by an evaluator
that is part of `kbf` and runs beside the farm [P]: it reads the farm's own metrics endpoint and invocation records, keeps
its state in a file (so a restart does not restart the clock), and writes the verdict.

**A bake with no usage is not a pass.** Real usage is the canary, so the bake has a minimum-load condition: at least
**[P]** 1000 executed actions and 10 builds across the window, and at least 3 distinct hours with load. Below that the
bake extends (it does not pass); the operator may waive it with a waiver that the verdict records.

Gates, evaluated continuously (a hard gate failing ends the bake red at once) and at the end:

| gate | measure | threshold | hard? |
|---|---|---|---|
| no infra-attributed failures | farm-class failed actions (the failed-actions-by-class counter, M10) | 0 over the bake | yes |
| lost blobs | `kbf_cache_lost_blobs` at every scrape; `kbf_cache_blob_losses_total` delta | gauge 0; delta 0 | yes |
| under-replicated | `under_replicated_blobs` | 0 at the end and 0 after the post-deploy settle window; above 0 for more than **[P]** 10 minutes outside a node step is a failure | yes |
| handover losses | `kbf_frontend_relay_lost_total` delta | 0 | yes |
| error rate | `kbf_execute_errors_total` / `kbf_execute_requests_total`, excluding client aborts once M11 exists | at most the larger of twice the baseline and the baseline plus **[P]** 0.5 percentage points | yes |
| latency | `Execute` queue-to-start p99 (`kbf_execute_queued_seconds`, a histogram **[F]**) against the baseline | not above **[P]** 1.5 times the baseline | no, reported; yes if above 3 times |
| write tier | flush failures, unflushed bytes, corrupt, `kbf_tier_flush_digest_mismatch_total` | no new failures or mismatches | yes |
| restarts | the updater's phase log until a process boot counter exists (M13) | no process restarted that the deploy did not restart | yes |

The **baseline** is the same ratios over the 7 days before the deploy, taken by the evaluator at deploy start and stored with
the verdict. An absent metric fails the gate, never passes it. Thresholds marked [P] are starting values; the bake's first
runs are measured, the thresholds are written down with the evidence, and a change to a threshold is a reviewed change to
this page and to the evaluator's table in the same commit.

`FindMissingBlobs` has request counters but no latency histogram **[F]**, so it has no latency gate until one is added (M14).
The evaluator is deliberately not a dashboard: a person reading a graph is not a gate and cannot be required.

### 7.1 The verdict

`gamma-verdict.json`: commit, set hash, desired generation, deploy start and end, bake start and end, each gate with measured value and
threshold, the minimum-load numbers and any waiver. It is a record, not a credential: no host reads it. A red verdict triggers
section 8; a green one makes the commit publishable (the tag of 8.1 carries it).

## 8. Stage 3: prod, and rollback

### 8.1 Prod: publish

Prod is `release.yml`, extended. **[F]** today: a pushed `v*` tag on a commit on main whose `artifacts.yml` push run exists;
the `verify` job checks the sums and every attestation; the `publish` job, the only one with `contents: write`, attaches the
already-built files and is idempotent; a dispatch with a commit is a dry-run verify. It has no notes, no environment approval
and no dependency on any earlier stage.

The additions:

1. **A verdict check in `verify`.** This is a release-process check in GitHub; it is **not host-update trust** (hosts never
   read the tag or the verdict, and verify only the artifact attestation). The tag is an annotated git tag on main whose
   message carries the gamma verdict (`gamma-verdict.json`). `verify` checks that the verdict names this commit, this set
   hash and this tag, that every gate is green, that the bake lasted at least 24 hours, and that nothing was waived without
   a recorded waiver. A tag with no verdict, or a red one, fails `verify`. **No signing key**: who may push a `v*` tag is
   limited by the tag-protection ruleset on main (M19), and the `publish` job is gated by the `prod` environment's
   required reviewer (item 2). A signing key would add key custody without adding protection beyond those two.
2. **A `prod` environment with a required reviewer**, as kci's prod, on the `publish` job.
3. **Release notes**: generated from the commits between the previous release and this one, plus the gamma verdict's summary
   (bake length, load, the gate values) and a list of anything that is not reversible (section 8.3). Notes are an artifact of
   the `verify` job, so a dry run shows them before anyone approves.
4. **Nothing is pushed to farms.** After `publish` the release is the attested files on the GitHub release. A farm that runs
   the public release finds them through its frontend's release poller, and its operator sets the desired commit (section 6).

Prod publishes the bytes gamma ran. If anything is rebuilt, `verify` fails: the set hash is the only thing it accepts.

### 8.2 Rollback

Rollback is the updater walking back, and it is the forward mechanism with the old release as the target.
(`split-mode-updates.md` section 2.11 **[F]**.)

- A rollback sets `update/desired` to the older release's commit, which takes a higher generation. Downgrades are allowed, and
  the generation is what stops a replayed older order. The old directories are still on the hosts (running, previous, one
  more), and the release rows and the disk release index still name the old bytes, which the host re-verifies against the
  attestation as for any release.
- Preflight runs; the frontend goes by a handover back (the old release started as the successor, only through Envoy, never
  restarting the serving one); nodes by the node step, idle or drained, in any order; across a protocol difference the order
  reverses (nodes first).
- **Triggers.** Local and fast: the commit-confirm auto-revert of a process that never becomes active (minutes, on the host,
  no controller needed). During a deploy: any gate of 5.2 halts and walks back what was changed. During a bake: a hard gate
  red makes the evaluator write a red verdict and **set the rollback automatically** when the release is reversible
  (8.3); when it is not, it pages and offers nothing. A soft gate never rolls back by itself.
- **Prod rollback** is the same move on each farm that runs the public release: its desired commit goes to the older release
  under a higher generation. The GitHub release is not rewritten (the published files are permanent, as kci's names are);
  the notes of the next release say what was withdrawn, and the release poller does not propose a withdrawn commit.
- **When a rollback lands mid-roll [P].** A process mid-step is not interrupted: the updater finishes that step to its
  phase boundary (T13), then walks back from the current state. The first node step and the frontend handover are each a
  place a rollback can arrive; B11 tests both, with N and N-1 mixed in the reverse order.
- **Who decides when the evaluator is down.** The evaluator that notices a red gate and the command that sets the desired
  commit are separate. If the evaluator is down the bake extends and pages (it never passes), and automatic rollback is
  unavailable; a person can set the older commit directly (`POST /v1/update/desired`), which does not depend on the evaluator. The only
  guarantee with both down is the host's own commit-confirm auto-revert, which is per process and minutes-scale.
- A rollback is tested in beta (B10, B11) and its result belongs to the same attested record.

### 8.3 Not reversible, and said before the click

From `split-mode-updates.md` **[F]**: a release that changed the schema beyond expand (the contract step of expand then
contract), and a node whose log version was raised (an older binary cannot open it; the walk-back wipes that node's meta
directory and rebuilds from segment footers). A release containing either is flagged at the offer: the offer command prints it, the
notes carry it, the evaluator's automatic rollback is off for it, and the contract step ships one release after the expand
step, never in it.

## 9. N/N-1 compatibility in CI

Nodes are updated asynchronously, so every pair of adjacent releases must work together in both directions for as long as a
window of `split-mode-updates.md` 3.1 allows. Two checks run on every merge, before beta's long scenarios:

- **Wire.** Compare the wire definitions (the protobuf descriptors, the handover protocol, the `done/` row tags, the
  `ServerMessage` arms) of the commit against the previous release's, and fail on a removed or renumbered field, a field made
  required, a repurposed tag, or a changed handover version without the flag-day marker. This is a lint, not a farm.
- **State store.** Keep a golden state-store file per release (written by that release's binary). The commit's binary opens
  the previous release's golden, and the previous release's binary opens a file the commit wrote (the expand-only case); a
  contract change is refused while a previous-release frontend is configured. T11 **[F, unbuilt]**.

Then B9 runs real processes: N frontend with N-1 nodes, N-1 frontend with N nodes (the downgrade order), a handover N-1 to N
and back, each with a build in flight (T10). The previous release comes from section 3. A release whose N-1 is unavailable
does not skip the check: the beta is red ("not run" is not "passed").

## 10. Failure handling, flake policy and result documents

- **A red beta is not retried to green silently.** One re-run of the same commit is allowed, to classify; both attempts are
  kept. Red then green is recorded as **flaky**, the green attempt alone does not pass the commit for promotion, and an issue
  is opened with both logs.
- **The assertions are not quarantinable.** A zero-failed-actions gate that flakes may be a real race in the update, which is
  what the stage exists to find. A flaky scenario setup (a port in use, a runner out of memory) is fixed in the harness. The
  way out of a stuck gate is a fix or a reverted commit, never a threshold edit or a waiver of a red beta.
- **Timeouts fail.** Every scenario has a wall-clock bound; a hang is red with the process list and logs attached.
- **Evidence is kept**: process logs, scraped metric series, event logs and the result document upload on every run for the
  same 90 days as the builds.
- **Gamma halts, it does not push through.** A halted deploy leaves a state the run can name, resumable from the host's phase
  (T13), never started from the top. An evaluator failure extends the bake and pages; it never counts as a pass.
- **A broken main**: a red full beta means no later commit is offered until a later commit has a green one.
- **Result documents.** One per stage, one shape (`stage`, `commit`, `set_hash`, `run_id`, `attempt`, `status`, `gates[]` with
  `name`, `measured`, `threshold`, `verdict`, `started`, `finished`), written RUNNING first and FINISHED on every exit, as kci
  does. Beta also posts a job summary (gate table, time per scenario, update timeline) and keeps a per-commit history file in
  the attested artifacts (beta duration, deploy length, bake values) so drift shows before a gate trips (M22).
- A scenario that only gamma found gets a beta scenario in the fix; the count of such escapes goes in the release notes.

## 11. Metrics the stages read

Exist **[F]**: `kbf_cache_lost_blobs`, `kbf_cache_blob_losses_total`, `kbf_execute_requests_total`, `kbf_execute_errors_total`,
`kbf_execute_in_flight`, `kbf_execute_queued_seconds` (histogram), `kbf_frontend_relay_lost_total`, the write-tier series.
Open pull requests **[open]**: `under_replicated_blobs`, open writes, repair debt, planned-down (#912, #913, #914).
**[missing]**: failed actions by class (M10), execute errors by cause (M11), a process boot counter and a membership-state
gauge (M13), a `FindMissingBlobs` latency histogram (M14).

## 12. Missing pieces and the issues to file

Status was read from feature/arch-v2 and the open pull requests when this page was written; re-check before use. Each row is
one issue with one done criterion. "Blocked by" names open pull requests or other rows.

| # | Missing piece | Done when | Blocked by | Issue |
|---|---|---|---|---|
| M1 | Harness library plus B-0: supervisor, pinned Envoy, real Buck2 load, B0, smoke in the merge queue | B0 green on a hosted runner on every merge; smoke measured, then required | #925; M21 |  #942 |
| M2 | Rolling update of every role under a running build (B1 to B4, B8) | each scenario red on its planted mutant and green; 0 failed actions, 0 lost blobs | M1, #912, #913, #914, #917, #930, #937, #938, #934 |  #943 |
| M3 | Kill a storage node and a daemon during a build (B5, B6); crash matrix (B7) required | scenarios green, matrix a required job | M1, #900, #939 |  #944 |
| M4 | N/N-1: wire lint, state-store goldens, mixed-version run both ways (B9) | lint refuses a planted removed field; B9 green | M1, M16 |  #945 |
| M5 | One frontend update process wired end to end (staged drain, relay, Envoy box, all-in-one) | B2 and B8 green | #937, #930, #925, #934, #938, #901 wiring PR |  #946 |
| M6 | Pull updater end to end: release poller, desired-state rows, host-side attestation verification, no SSH push | the updater replaces the stand-in in B1 to B4 | #908, #929, M7 |  #947 |
| M7 | `offer`: promotion by setting the desired commit, beta-result check, plan mode (no operator-signed feed) | `offer` refuses on a red or missing beta result and on an incomplete release; the generation only rises | |  #948 |
| M8 | Gamma deploy-as-test checks: in-flight invocation record, dropped-connection and lost-blob checks, halt and walk back | a planted dropped connection fails the deploy | M6, #791, #792 |  #949 |
| M9 | Bake evaluator: baseline, minimum load, state file, verdict | a restart keeps the clock; an absent metric fails; thin usage extends | M10, M13 |  #950 |
| M10 | Metric: failed actions by failure class | counter present, labelled by class | |  #951 |
| M11 | Metric: Execute errors by cause | client abort distinguished from dropped | |  #952 |
| M12 | Release workflow: verdict check on the release tag (commit, set hash and tag bound; no signing key), approval environment, notes | a tag with no verdict, a red one, or a verdict naming another commit, set or tag fails `verify` | M7, M9, M19 |  #953 |
| M13 | Metrics: process boot counter and membership-state gauge | both exported; gates read them | |  #954 |
| M14 | Metric: `FindMissingBlobs` latency histogram | histogram exported; latency gate added | |  #955 |
| M15 | Updater walk-back: automatic desired-commit set on a red hard gate, mid-roll behaviour, not-reversible flags (B10, B11) | B10 and B11 green | M6, M7 |  #956 |
| M16 | Fetch and verify the previous release for N-1 | N-1 resolved, or the fallback of section 3 | |  #957 |
| M17 | Stage file and lint (stages, no `pull_request`, `id-token` only in `attest`) | lint red on each planted violation | |  #958 |
| M18 | Rollout engine (#803) push transport replaced by the pull updater's, or retired | no push transport left | #803 |  #959 |
| M19 | Repo settings: required checks and merge rules on feature/arch-v2 (the maintainers' choice); the tag-protection ruleset for `v*` on main once release cuts start | settings in place | M1 |  #960 |
| M20 | Decision: releases are cut from main only, after feature/arch-v2 merges to main (Q1, decided) | decision recorded | |  #961 (closed) |
| M21 | Measure the whole farm on a standard hosted runner (peak memory, wall time, safe concurrency) | numbers recorded here | |  #962 |
| M22 | Result documents, flake classification and history file | one shape, flaky recorded, history written | M1 |  #963 |
| M23 | crun with cgroup v2 delegation on a hosted runner **[V]** | a real container action runs under crun in CI | #936 |  #964 |

Links, not issues: #912, #913, #914 (replication metrics), #936 (container runtime: crun only), #791, #792, #847 (the
dependencies of every "0 failed actions" gate).

## 13. Open questions, with recommendations

1. **Branch convergence for prod. Decided (#961): main only.** No release is cut from feature/arch-v2. Development
   continues there until the farm works, then the branch merges to main (#753), and every release from then on is cut from
   main. The merge makes a new commit, so bytes attested on the feature branch are never what prod publishes: the first
   release is built on main and runs beta and gamma on that commit.
2. **Required checks. Decided (#960): the maintainers choose the feature branch's required checks and merge rules.**
   The recommendation stands as input: the smoke as a required check on feature/arch-v2 (merges on green Linux checks), the
   full run non-blocking for merge but blocking for the offer. The tag-protection ruleset for `v*` applies to main once
   release cuts start.
3. **Where the bytes live for gamma.** Decided with the pull updater (#927): the frontend's release store and disk release
   index hold the bytes, filled by its release poller from the GitHub release; there is no feed to host. Whether gamma
   should keep working when GitHub is unreachable is answered by that store (a release already stored is deployable).
4. **Verdict channel. Decided: the release tag's message, unsigned.** The verdict travels in the annotated `v*` tag on main;
   the tag-protection ruleset limits who can push it and the `prod` environment reviewer gates `publish`. No signing key
   (it would add custody with no new protection).
5. **Does the operator's farm need to be the only gamma?** One farm's usage can be too thin to be a canary. The minimum-load
   rule makes that visible (the bake extends). *Recommend keeping one gamma now*, and revisiting if bakes routinely extend.
6. **Automatic rollback during a bake.** *Recommend automatic offer for hard gates on a reversible release, a page otherwise*,
   with a person able to cancel before the desired commit is set (a short, **[P]** 10 minute hold). Owner decision.
7. **Hosted runner capacity.** *Recommend measuring B-0 first* (peak memory and wall time of the whole farm on a standard
   runner) and deciding on a larger hosted runner only on that number. Self-hosted only under the conditions of 4.2.
8. **Baseline for the error-rate gate** over 7 days versus the previous release's bake. *Recommend 7 days*, since the
   previous bake is one data point and may itself have been bad.
9. **Where the harness sits when #936 lands.** The stand-in runtime goes. *Recommend B-0 on the stand-in with the weakening
   stated (section 4.2)*, and a crun-with-delegation check on `ubuntu-24.04` as its own item, so beta is not delayed by #936.
10. **Length.** The page is about 590 lines against a 300 to 500 target. Review asked for fixes that add content (the
    missing-metric rows, the rollback cases), so the minor request to trim was only partly taken (sections 10
    and 11 were folded). *Recommend accepting the length*; the table in section 12 is most of it.

## 14. What this page changes in other documents

None yet. When the pieces land: `docs/artifacts.md` (the deployment verify job it says is planned becomes the host's attestation check and the offer's),
`split-mode-updates.md` (section 7's test IDs get the CI job that runs them), and the workflow lints (section 2.1).
