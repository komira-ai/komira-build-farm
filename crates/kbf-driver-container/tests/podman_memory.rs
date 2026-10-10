//! The memory policy against real rootless Podman and a real cgroup v2 kernel: each
//! lease's hard cap (`memory.max` = booking x 1.5 + 512 MiB) with `memory.oom.group=1`,
//! swap left allowed, the driver's swap watch (`SwapKill`), and the classification of
//! an OOM kill by the lease cgroup's `memory.events`: at the lease's own cap it is
//! `OutOfMemory`; without it (the `actions/` backstop, or the host) it is `BusyNode`.
//!
//! Set up as `tests/podman.rs` is (see there): `tools/ci/podman-tests.sh` runs these
//! with `--include-ignored` in a unit with `Delegate=yes`.
//!
//! Each test's cell cgroup stands in for `actions/`. Where a test needs the kernel to
//! OOM-kill, its cell sets `memory.swap.max=0`: a node whose swap is full. With swap
//! free, a lease at its cap is reclaimed into swap (the policy sets no per-lease
//! `memory.swap.max`), and the driver's swap watch kills it
//! ([`past_its_cap_with_swap_free_the_driver_kills_it_as_out_of_memory`]); a lease
//! swapped below its cap is not killed ([`swapped_below_its_cap_a_lease_is_not_killed`]),
//! nor is one swapped earlier that then touches its cap without its swap rising
//! ([`swapped_by_the_host_then_at_its_cap_a_lease_is_not_killed`]).
//! The tests that need swap fail on a node without it.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::SwapKill;
use kbf_types::{LeaseId, Resources};
use support::real::{Cell, describe, podman, sh};
use support::store_action;

const MIB: u64 = 1 << 20;

/// The cap `memory.max` the driver gives a lease that booked `booked` bytes.
fn cap(booked: u64) -> u64 {
    booked + booked / 2 + 512 * MIB
}

fn write(dir: &Path, file: &str, value: &str) {
    std::fs::write(dir.join(file), value).unwrap_or_else(|e| panic!("write {file}: {e}"));
}

fn read_u64(dir: &Path, file: &str) -> u64 {
    let text = std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    text.trim()
        .parse()
        .unwrap_or_else(|_| panic!("{file}: {text:?}"))
}

/// Fails the test unless the node has swap.
fn require_swap() {
    let swap_kib: u64 = std::fs::read_to_string("/proc/meminfo")
        .expect("meminfo")
        .lines()
        .find_map(|l| l.strip_prefix("SwapTotal:"))
        .and_then(|v| v.trim().strip_suffix("kB")?.trim().parse().ok())
        .expect("SwapTotal");
    assert!(
        swap_kib > 0,
        "this test needs a node with swap; this one has none"
    );
}

/// What the lease cgroup `lease` holds and counts now, for a failure message.
fn state(lease: &Path) -> String {
    let read = |f: &str| {
        std::fs::read_to_string(lease.join(f))
            .map_or_else(|e| format!("({e})"), |t| t.trim().replace('\n', ","))
    };
    format!(
        "memory.current {}, memory.swap.current {}, memory.events {}",
        read("memory.current"),
        read("memory.swap.current"),
        read("memory.events")
    )
}

/// Waits until a process whose command name is `comm` runs anywhere under `lease`.
async fn wait_for(lease: &Path, comm: &str) {
    for _ in 0..1200 {
        let mut pending = vec![lease.to_owned()];
        while let Some(dir) = pending.pop() {
            let procs = std::fs::read_to_string(dir.join("cgroup.procs")).unwrap_or_default();
            if procs.split_whitespace().any(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == comm)
            }) {
                return;
            }
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    pending.push(entry.path());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("no {comm} under {}:{}", lease.display(), describe(lease));
}

/// Catches a lease killed past its cap reported as anything but `OutOfMemory` with its
/// cap, an OOM kill that takes a neighbour lease with it, and a missing OOM group (the
/// "oom.group off" mutant, on the lease and the container: the kernel kills only `dd`,
/// the action's other child `sleep 3600` lives on, and the run ends at its timeout).
///
/// Lease 1 books 16 MiB (cap 536 MiB) and reads 700 MiB into one buffer; lease 2 sleeps
/// beside it, in the same `actions/`, and must still be running afterwards.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn past_its_cap_an_action_dies_alone_and_is_out_of_memory() {
    let cell = Cell::new("own-cap");
    write(&cell.cgroup, "memory.swap.max", "0");

    let neighbour = store_action(&cell.cas, &sh("exec sleep 120"));
    let work = cell.work(2, neighbour, Resources::new(1000, 16 * MIB));
    let runtime = Arc::clone(&cell.runtime);
    let second = tokio::spawn(async move { runtime.run(work).await });
    wait_for(&cell.cgroup.join(cell.name(2)), "sleep").await;

    let mut spec = sh("sleep 3600 & dd if=/dev/zero of=/dev/null bs=700M count=1; wait");
    spec.timeout = Some(Duration::from_secs(60));
    let action = store_action(&cell.cas, &spec);
    let outcome = cell
        .runtime
        .run(cell.work(1, action, Resources::new(1000, 16 * MIB)))
        .await;
    let want = cap(16 * MIB);
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used, limit }) if limit == want && used > 16 * MIB),
        "{outcome:?}"
    );
    cell.assert_clean(1);

    let running = podman(&["ps", "--format={{.Names}}"]);
    assert!(
        running.lines().any(|n| n == cell.name(2)),
        "the neighbour lease died with the one past its cap: {running}"
    );
    cell.runtime.kill(LeaseId::new(cell.term, 2)).await;
    let outcome = second.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    cell.assert_clean(2);
}

/// Catches a busy node's kill read as the action's own (the "classify by exit code
/// alone" mutant: both end with SIGKILL, 137), which would double the booking of an
/// action that never reached its cap. The action books 1 GiB (cap 2 GiB) and reads
/// 200 MiB; the cell, standing in for `actions/`, allows 64 MiB, so the kernel's OOM
/// killer runs for the cell's limit, not the lease's.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn killed_by_the_actions_backstop_within_its_cap_is_a_busy_node() {
    let cell = Cell::new("backstop");
    write(&cell.cgroup, "memory.max", "64M");
    write(&cell.cgroup, "memory.swap.max", "0");
    let action = store_action(
        &cell.cas,
        &sh("dd if=/dev/zero of=/dev/null bs=200M count=1"),
    );
    let outcome = cell
        .runtime
        .run(cell.work(1, action, Resources::new(1000, 1 << 30)))
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("oom 0, max 0")
                && why.contains("time(s) for the actions/ cgroup's own limit during the lease")),
        "{outcome:?}"
    );
    cell.assert_clean(1);
}

/// Catches a cap with no buffer above the booking (the "cap = booking" mutant): an
/// action that books 64 MiB and holds 300 MiB, past its booking and within its cap of
/// 608 MiB, runs to exit 0 with no swap to fall back on.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn past_its_booking_within_its_cap_an_action_runs() {
    let cell = Cell::new("buffer");
    write(&cell.cgroup, "memory.swap.max", "0");
    let action = store_action(
        &cell.cas,
        &sh("dd if=/dev/zero of=/dev/null bs=300M count=1"),
    );
    let result = cell
        .runtime
        .run(cell.work(1, action, Resources::new(1000, 64 * MIB)))
        .await
        .expect("ran within its cap");
    assert_eq!(result.exit_code, 0, "{result:?}");
    cell.assert_clean(1);
}

/// Catches a lease past its cap left to swap (the "watch off" mutant: the action
/// holds its buffer until its 60 s timeout), a kill reported as anything but the
/// lease's own out-of-memory kill (the "busy node for an own-cap kill" mutant), a kill
/// that is slow, and one that takes a neighbour lease with it.
///
/// With swap free, lease 1 books 16 MiB (cap 536 MiB) and fills one 900 MiB buffer
/// (`dd` then blocks writing it into a pipe nobody reads), so reclaim at its cap moves
/// some 360 MiB of it to swap, past the 128 MiB floor this cell's watch is given; the
/// watch samples every second. Lease 2 sleeps beside it and must still be running.
/// The buffer is random, not zeros: a kernel that splits huge pages under reclaim
/// (podman-x86's) drops zero-filled pages for the shared zero page, so a buffer of
/// zeros shrinks below the cap and never reaches swap.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn past_its_cap_with_swap_free_the_driver_kills_it_as_out_of_memory() {
    require_swap();
    let cell = Cell::with("swap-kill", |config| {
        config.swap_kill = SwapKill {
            floor_bytes: 128 * MIB,
            ..SwapKill::DEFAULT
        };
    });

    let neighbour = store_action(&cell.cas, &sh("exec sleep 120"));
    let work = cell.work(2, neighbour, Resources::new(1000, 16 * MIB));
    let runtime = Arc::clone(&cell.runtime);
    let second = tokio::spawn(async move { runtime.run(work).await });
    wait_for(&cell.cgroup.join(cell.name(2)), "sleep").await;

    let mut spec = sh("dd if=/dev/urandom bs=900M count=1 iflag=fullblock | sleep 3600");
    spec.timeout = Some(Duration::from_secs(60));
    let action = store_action(&cell.cas, &spec);
    let started = Instant::now();
    let runtime = Arc::clone(&cell.runtime);
    let work = cell.work(1, action, Resources::new(1000, 16 * MIB));
    let mut first = tokio::spawn(async move { runtime.run(work).await });
    // What the lease looked like each second, for the failure message.
    let lease = cell.cgroup.join(cell.name(1));
    let mut seen = Vec::new();
    let outcome = loop {
        tokio::select! {
            outcome = &mut first => break outcome.expect("join"),
            () = tokio::time::sleep(Duration::from_secs(1)) => {
                seen.push(format!("{:?}: {}", started.elapsed(), state(&lease)));
            }
        }
    };
    let took = started.elapsed();
    let want = cap(16 * MIB);
    // `used` is RAM plus swap at the kill: past the cap by more than the floor.
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used, limit })
            if limit == want && used > want + 128 * MIB),
        "{outcome:?}\n{}",
        seen.join("\n")
    );
    assert!(took < Duration::from_secs(20), "killed after {took:?}");
    println!("killed {took:?} after the start: {outcome:?}");
    cell.assert_clean(1);

    let running = podman(&["ps", "--format={{.Names}}"]);
    assert!(
        running.lines().any(|n| n == cell.name(2)),
        "the neighbour lease died with the one past its cap: {running}"
    );
    cell.runtime.kill(LeaseId::new(cell.term, 2)).await;
    let outcome = second.await.expect("join");
    assert!(matches!(outcome, Err(RuntimeError::Killed)), "{outcome:?}");
    cell.assert_clean(2);
}

/// Catches the swap watch killing on swap alone (the "ignores max events" mutant), as
/// host pressure would have it: a lease well below its cap holds a 150 MiB buffer of
/// random bytes (`dd` blocked writing it into a pipe nobody reads yet; random, so
/// reclaim cannot drop it as zero pages), the test reclaims 100 MiB
/// from it (`memory.reclaim`, the kernel's reclaim run on that one cgroup, standing in
/// for the host's), so its `memory.swap.current` rises with no `max` event, past the
/// 8 MiB floor this cell's watch is given, and the action still exits 0 after the watch
/// has sampled it for several seconds. Also catches a lease that reclaim cannot move
/// to swap (a per-lease `memory.swap.max=0` mutant).
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn swapped_below_its_cap_a_lease_is_not_killed() {
    require_swap();
    const FLOOR: u64 = 8 * MIB;
    let cell = Cell::with("swap", |config| {
        config.swap_kill = SwapKill {
            floor_bytes: FLOOR,
            percent: 0,
            every: Duration::from_millis(200),
        };
    });
    let spec =
        sh("dd if=/dev/urandom bs=150M count=1 iflag=fullblock | { sleep 10; cat >/dev/null; }");
    let action = store_action(&cell.cas, &spec);
    let work = cell.work(1, action, Resources::new(1000, 256 * MIB));
    let runtime = Arc::clone(&cell.runtime);
    let run = tokio::spawn(async move { runtime.run(work).await });
    let lease = cell.cgroup.join(cell.name(1));
    wait_for(&lease, "sleep").await;
    // The buffer is anonymous memory: `anon` in `memory.stat`, not the file's cache.
    let anon = |lease: &Path| {
        std::fs::read_to_string(lease.join("memory.stat"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("anon ")?.parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    let mut held = 0;
    for _ in 0..100 {
        held = anon(&lease);
        if held >= 150 * MIB {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held >= 150 * MIB, "the buffer is not held: {held} bytes");
    // EAGAIN when it reclaimed less than asked; what it moved is read below.
    let _ = std::fs::write(lease.join("memory.reclaim"), "200M");
    let swapped = read_u64(&lease, "memory.swap.current");
    assert!(
        swapped > FLOOR,
        "the premise: the lease holds more than the watch's floor in swap: {}",
        state(&lease)
    );
    let events = std::fs::read_to_string(lease.join("memory.events")).expect("memory.events");
    assert!(
        events.lines().any(|l| l == "max 0"),
        "the premise: the lease never reached its cap: {events}"
    );
    let result = run.await.expect("join").expect("ran");
    assert_eq!(result.exit_code, 0, "{result:?}");
    println!("{swapped} bytes of the lease in swap; it ran to exit 0");
    cell.assert_clean(1);
}

/// Catches the swap watch killing a lease whose swap did not rise (the "swap rose
/// dropped" mutant): a lease that host pressure swapped out earlier, and that then
/// touches its own cap without pushing more into swap, runs to exit 0.
///
/// The lease books 256 MiB, holds a 150 MiB buffer of random bytes (`dd` blocked
/// writing it into a pipe nobody reads) and writes a 64 MiB file into its working
/// directory, synced, so its pages are clean. The test reclaims 200 MiB from the lease
/// (`memory.reclaim`, standing in for host pressure: no `max` event): the file's pages
/// are dropped and much of the buffer goes to swap, past the 8 MiB floor this cell's
/// watch is given. It then pins the lease's `memory.swap.max` to what the lease holds
/// in swap now, so nothing more of it can go there, and lowers the lease's
/// `memory.max` to 16 MiB above what it holds in RAM. The action then reads the file,
/// over and over: its page cache hits the cap (the `max` events grow), and reclaim at
/// the cap drops the clean file pages. The rule without "swap rose" kills it at the
/// first sample after that (`max` grew, swap above the floor). Clean pages, not
/// written ones: a lease at so tight a cap that writes is OOM-killed by the kernel
/// (seen on both podman jobs) before its dirty pages are written back.
#[tokio::test]
#[ignore = "needs rootless Podman and a delegated cgroup: run by tools/ci/podman-tests.sh"]
async fn swapped_by_the_host_then_at_its_cap_a_lease_is_not_killed() {
    require_swap();
    const FLOOR: u64 = 8 * MIB;
    let cell = Cell::with("swap-held", |config| {
        config.swap_kill = SwapKill {
            floor_bytes: FLOOR,
            percent: 0,
            every: Duration::from_millis(200),
        };
    });
    let mut spec = sh(
        "dd if=/dev/urandom bs=150M count=1 iflag=fullblock 2>/dev/null | { \
         dd if=/dev/zero of=f bs=1M count=64 2>/dev/null; sync; sleep 10; i=0; \
         while [ $i -lt 20 ]; do cat f >/dev/null; i=$((i + 1)); done; rm -f f; }",
    );
    spec.timeout = Some(Duration::from_secs(120));
    let action = store_action(&cell.cas, &spec);
    let work = cell.work(1, action, Resources::new(1000, 256 * MIB));
    let runtime = Arc::clone(&cell.runtime);
    let mut run = tokio::spawn(async move { runtime.run(work).await });
    let lease = cell.cgroup.join(cell.name(1));
    wait_for(&lease, "sleep").await;
    // The buffer is anonymous memory: `anon` in `memory.stat`, not the file's cache.
    let anon = |lease: &Path| {
        std::fs::read_to_string(lease.join("memory.stat"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("anon ")?.parse::<u64>().ok())
            })
            .unwrap_or(0)
    };
    let mut held = 0;
    for _ in 0..100 {
        held = anon(&lease);
        if held >= 150 * MIB {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(held >= 150 * MIB, "the buffer is not held: {held} bytes");
    // EAGAIN when it reclaimed less than asked; what it moved is read below.
    let _ = std::fs::write(lease.join("memory.reclaim"), "200M");
    let swapped = read_u64(&lease, "memory.swap.current");
    assert!(
        swapped > FLOOR,
        "the premise: host pressure moved more than the watch's floor to swap: {}",
        state(&lease)
    );
    write(&lease, "memory.swap.max", &swapped.to_string());
    let ram = read_u64(&lease, "memory.current");
    write(&lease, "memory.max", &(ram + 16 * MIB).to_string());
    let max_events = |lease: &Path| {
        std::fs::read_to_string(lease.join("memory.events"))
            .ok()
            .and_then(|t| {
                t.lines()
                    .find_map(|l| l.strip_prefix("max ")?.parse::<u64>().ok())
            })
    };
    assert_eq!(
        max_events(&lease),
        Some(0),
        "the premise: the lease had not reached its cap before: {}",
        state(&lease)
    );

    // What the lease counted while the action read at its cap, for the premise: the
    // least swap it held in a sample whose `max` events grew since the one before
    // (after the action exits, the lease cgroup lives on with nothing in swap).
    let mut seen = Vec::new();
    let mut at_cap = 0;
    let mut least_swap = u64::MAX;
    let outcome = loop {
        tokio::select! {
            outcome = &mut run => break outcome.expect("join"),
            () = tokio::time::sleep(Duration::from_millis(200)) => {
                let swap = std::fs::read_to_string(lease.join("memory.swap.current"))
                    .ok()
                    .and_then(|t| t.trim().parse::<u64>().ok());
                if let Some(max) = max_events(&lease) {
                    if let (true, Some(swap)) = (max > at_cap, swap) {
                        least_swap = least_swap.min(swap);
                    }
                    at_cap = at_cap.max(max);
                }
                seen.push(state(&lease));
            }
        }
    };
    let result = outcome.unwrap_or_else(|e| panic!("{e:?}\n{}", seen.join("\n")));
    assert_eq!(result.exit_code, 0, "{result:?}\n{}", seen.join("\n"));
    assert!(
        at_cap > 0,
        "the premise: the action reached its cap: {}",
        seen.join("\n")
    );
    assert!(
        least_swap > FLOOR && least_swap != u64::MAX,
        "the premise: the lease held more than the floor in swap while at its cap: {}",
        seen.join("\n")
    );
    println!(
        "{swapped} bytes in swap from the reclaim, {at_cap} max events at the cap; it ran to exit 0"
    );
    cell.assert_clean(1);
}
