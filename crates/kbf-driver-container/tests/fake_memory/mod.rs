//! How the driver reports a lease killed for memory, against the stand-in `crun`
//! (`support/fake.rs`) and a plain directory standing in for the cgroup mount:
//! the action's act steps write the lease cgroup's `memory.events`, `memory.current` and
//! `memory.swap.current` as the kernel would. `crun_memory.rs` runs the same promises
//! against a real kernel.
//!
//! A module of `fake_crun.rs`, not a test binary of its own. The coverage job counts
//! the lines of the driver's generic `ContainerRuntime` per build of it; split across two
//! test binaries, the swap watch's paths (run here) and the timeout and kill paths (run
//! in `fake_crun.rs`) each counted as missed in the other binary's build.

use std::os::unix::fs::PermissionsExt as _;
use std::time::{Duration, Instant};

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::SwapKill;
use kbf_types::Resources;

use crate::support::fake::{Fake, image};
use crate::support::swap_kill::{Logs, assert_swap_kill};
use crate::support::{Spec, store_action, work};

/// The cap `Fake::run`'s 1 GiB booking gets: 1.5 GiB + 512 MiB.
const CAP: u64 = (3 << 29) + (512 << 20);

/// The act steps of an action here, added one at a time.
#[derive(Default)]
struct Steps(Vec<String>);

impl Steps {
    /// Adds the step `words`.
    fn then(mut self, words: &[&str]) -> Self {
        if !self.0.is_empty() {
            self.0.push(";".to_owned());
        }
        self.0.extend(words.iter().map(|word| (*word).to_owned()));
        self
    }

    /// Writes `lines`, each with a newline, to the lease cgroup's `file`, as the kernel
    /// would update it.
    fn cgroup(self, file: &str, lines: &[&str]) -> Self {
        let path = format!("$CG/{file}");
        let mut words = vec!["write-lines", path.as_str()];
        words.extend(lines);
        self.then(&words)
    }

    /// Writes `memory.events` whole: a new file renamed over it, so the swap watch
    /// never reads half of it.
    fn events(self, lines: &[&str]) -> Self {
        self.cgroup("memory.events.new", lines).then(&[
            "rename",
            "$CG/memory.events.new",
            "$CG/memory.events",
        ])
    }

    fn sleep(self, secs: &str) -> Self {
        self.then(&["sleep", secs])
    }

    /// The steps, ending with exit status `code`.
    fn exit(self, code: u8) -> Vec<String> {
        self.then(&["exit", code.to_string().as_str()]).0
    }
}

/// Catches a kernel OOM kill reported as the action's own exit 137 (it would be cached
/// as a failing action), an action's own exit 137 reported as an OOM, and the two
/// kinds of OOM kill not told apart by the lease cgroup's counters: a kill whose `oom`
/// and `max` the lease counts is the lease's own (OutOfMemory, with `memory.peak` and
/// the cap), one with `oom_kill` and no `oom` is a busy node's, whatever the exit
/// code says (the "classify by exit code alone" mutant reads both the same).
#[tokio::test]
async fn exit_137_is_an_oom_only_with_a_kernel_oom_event() {
    let fake = Fake::new("oom");
    let spec = Spec::new(&image(), "unused");
    // `Fake::run` books 1 GiB.
    let cap = (3u64 << 29) + (512 << 20);
    let oom = Steps::default()
        .cgroup(
            "memory.events",
            &["low 0", "high 4", "max 1", "oom 1", "oom_kill 1"],
        )
        .cgroup("memory.peak", &["2000000000"])
        .exit(137);
    let outcome = fake.run(1, &spec, &oom).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 2_000_000_000, limit }) if limit == cap),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    // Killed by an ancestor's OOM: `oom_kill` in the lease, its own `oom` never.
    let busy = Steps::default()
        .cgroup("memory.events", &["max 0", "oom 0", "oom_kill 3"])
        .exit(137);
    let outcome = fake.run(2, &spec, &busy).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why)) if why.contains("killed 3 process")),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    let own = Steps::default()
        .cgroup("memory.events", &["max 0", "oom 0", "oom_kill 0"])
        .exit(137);
    let result = fake.run(3, &spec, &own).await.expect("ran");
    assert_eq!(result.exit_code, 137);
    fake.assert_clean(3);

    // No memory.events to read: the driver cannot tell, so it does not guess.
    let outcome = fake.run(4, &spec, &["exit", "137"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("memory.events")),
        "{outcome:?}"
    );
    fake.assert_clean(4);

    // Nor when memory.events carries no count.
    let garbled = Steps::default()
        .cgroup("memory.events", &["max 0", "oom 0", "oom_kill many"])
        .exit(137);
    let outcome = fake.run(5, &spec, &garbled).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("no oom_kill count")),
        "{outcome:?}"
    );
    fake.assert_clean(5);
}

/// Catches a busy node's kill that does not say whether `actions/`'s own limit ran the
/// OOM killer during the lease (its `memory.events.local` `oom` grew), and an
/// out-of-memory kill whose `memory.peak` cannot be read reported with a usage below
/// the cap it reached.
#[tokio::test]
async fn a_memory_kill_says_whose_limit_ran_out() {
    let fake = Fake::new("oom-whose");
    let spec = Spec::new(&image(), "unused");
    let local = fake.cgroup.join("actions/memory.events.local");
    let busy = || Steps::default().cgroup("memory.events", &["max 0", "oom 0", "oom_kill 1"]);

    std::fs::write(&local, "low 0\noom 4\noom_kill 9\n").expect("write");
    let grew = busy()
        .cgroup("../memory.events.local", &["oom 6"])
        .exit(137);
    let outcome = fake.run(1, &spec, &grew).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("ran 2 time(s) for the actions/ cgroup's own limit")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let outcome = fake.run(2, &spec, &busy().exit(137)).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("did not run for the actions/ cgroup's own limit")),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    std::fs::write(&local, "garbled\n").expect("write");
    let outcome = fake.run(3, &spec, &busy().exit(137)).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("memory.events.local could not be read")),
        "{outcome:?}"
    );
    fake.assert_clean(3);

    let cap = (3u64 << 29) + (512 << 20);
    let own = Steps::default()
        .cgroup("memory.events", &["max 2", "oom 1", "oom_kill 1"])
        .cgroup("memory.peak", &["n/a"])
        .exit(137);
    let outcome = fake.run(4, &spec, &own).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used, limit }) if used == cap && limit == cap),
        "{outcome:?}"
    );
    fake.assert_clean(4);
}

/// Catches the lease cgroup's counters read only on exit 137: when `crun` reported an
/// error and no exit (`NotRun`), a kill the counters show is still reported as the memory
/// kill it was, by whose limit, and not as `crun`'s failure. Without a kill in the
/// counters, what `crun` said is reported.
#[tokio::test]
async fn a_memory_kill_without_an_exit_from_crun_is_still_reported() {
    let fake = Fake::new("oom-no-exit");
    let spec = Spec::new(&image(), "unused");
    let own = Steps::default()
        .cgroup("memory.events", &["max 2", "oom 1", "oom_kill 1"])
        .cgroup("memory.peak", &["5000000000"])
        .exit(137);
    let busy = Steps::default()
        .cgroup("memory.events", &["max 0", "oom 0", "oom_kill 2"])
        .exit(137);

    fake.knob("error-after-run", "cannot clean up the container");
    let outcome = fake.run(1, &spec, &own).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 5_000_000_000, limit }) if limit == CAP),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    let outcome = fake.run(2, &spec, &busy).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why)) if why.contains("killed 2 process")),
        "{outcome:?}"
    );
    fake.assert_clean(2);
    // No kill in the counters: `crun`'s word stands.
    let outcome = fake.run(3, &spec, &["exit", "137"]).await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("did not run to an exit")),
        "{outcome:?}"
    );
    fake.assert_clean(3);
}

/// A fake whose swap watch samples every 20 ms with `floor` bytes and `percent`.
fn watched(name: &str, floor: u64, percent: u64) -> Fake {
    Fake::with(name, |config| {
        config.swap_kill = SwapKill {
            floor_bytes: floor,
            percent,
            every: Duration::from_millis(20),
        };
    })
}

/// How an action's `max` events and swap move while it holds memory.
#[derive(Clone, Copy)]
enum Cap {
    /// They grow every 10 ms from the start, its swap already out when the watch
    /// first reads it: it keeps hitting its own cap, pressing into swap.
    Pressed,
    /// They stay 0: it never reached its cap.
    Never,
    /// They are 2 from the start and stay there: it hit its cap once, and only after
    /// the watch has seen that does anything go to swap.
    Once,
    /// As [`Cap::Once`], then from about 400 ms in its `max` events grow again while
    /// its swap stays where it is: host pressure swapped it out earlier, and it now
    /// touches its cap without pushing more into swap.
    HeldThenPressed,
}

/// An action that holds 5000 bytes in RAM and `swap` in swap (with [`Cap::Once`] and
/// [`Cap::HeldThenPressed`], from about 200 ms in), its `max` events moving as `cap`
/// says. It ends with exit 0 after about a second unless something kills it.
fn holding(swap: u64, cap: Cap) -> Vec<String> {
    // The step from which `max` grows, and the step at which the swap goes out.
    let (start, grow_from, swap_at) = match cap {
        Cap::Pressed => (2, 0, 0),
        Cap::Never => (0, 1000, 0),
        Cap::Once => (2, 1000, 20),
        Cap::HeldThenPressed => (2, 40, 20),
    };
    let first = if swap_at == 0 { swap } else { 0 };
    let mut steps = Steps::default()
        .cgroup("memory.current", &["5000"])
        .cgroup("memory.swap.current", &[first.to_string().as_str()]);
    let mut max = start;
    for n in 0..100 {
        if n == swap_at {
            steps = steps.cgroup("memory.swap.current", &[swap.to_string().as_str()]);
        }
        steps = steps.events(&[format!("max {max}").as_str(), "oom 0", "oom_kill 0"]);
        if n >= grow_from {
            max += 1;
        }
        steps = steps.sleep("0.01");
    }
    steps.exit(0)
}

/// Catches the swap watch missing (the over-cap lease runs on: exit 0), a kill
/// reported as anything but the lease's own out-of-memory kill with what it held in
/// RAM and swap, and a kill that is not `cgroup.kill` on the lease (its processes left
/// to the clean). The lease keeps hitting its cap with 1 MiB in swap, past the
/// configured floor of 64 KiB.
#[tokio::test]
async fn a_lease_pressing_its_cap_into_swap_is_killed_as_out_of_memory() {
    let fake = watched("swap-kill", 64 << 10, 0);
    let started = Instant::now();
    let outcome = fake
        .run(
            1,
            &Spec::new(&image(), "unused"),
            &holding(1 << 20, Cap::Pressed),
        )
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used, limit })
            if used == 5000 + (1 << 20) && limit == CAP),
        "{outcome:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    assert!(
        fake.events()
            .iter()
            .any(|e| e == "cgroup.kill ended the action"),
        "{:?}",
        fake.events()
    );
    fake.assert_clean(1);
}

/// An action whose RAM sits 1 MiB below the cap (reclaim at the cap has taken it below)
/// while each 50 ms its `max` events grow and 256 KiB more goes to swap: all of its swap
/// is its own. It ends with exit 0 after about 5 s unless something kills it. The
/// swap file reads `n/a` while `max` moves, so no sample sees swap rise with `max` flat.
fn reclaimed_below_the_cap() -> Vec<String> {
    const STEP: u64 = 256 << 10;
    let mut steps = Steps::default()
        .cgroup("memory.current", &[(CAP - (1 << 20)).to_string().as_str()])
        .cgroup("memory.swap.current", &["0"]);
    for n in 1..=100 {
        steps = steps
            .cgroup("memory.swap.current", &["n/a"])
            .events(&[format!("max {n}").as_str(), "oom 0", "oom_kill 0"])
            .cgroup("memory.swap.current", &[(n * STEP).to_string().as_str()])
            .sleep("0.05");
    }
    steps.exit(0)
}

/// Catches a kill check that wants RAM plus swap past the cap by the threshold (the
/// check the arm job failed on a correct kill): at the kill sample this lease holds
/// 1 MiB less than its cap in RAM and 1.25 MiB of its own swap, past the 1 MiB floor,
/// so it is past its cap by only 256 KiB. Also catches the watch missing (exit 0) and a
/// threshold that drops the floor (the "0 floor" mutant kills at the first 256 KiB of
/// own swap, below the cap and the floor).
#[tokio::test]
async fn a_lease_reclaimed_below_its_cap_is_killed_for_its_own_swap() {
    let fake = watched("swap-below", 1 << 20, 0);
    let logs = Logs::capture();
    let outcome = fake
        .run(
            1,
            &Spec::new(&image(), "unused"),
            &reclaimed_below_the_cap(),
        )
        .await;
    assert_swap_kill(&outcome, &logs.text(), CAP, 1 << 20, "");
    drop(logs);
    fake.assert_clean(1);
}

/// Catches the watch killing on swap alone (the "ignores max events" mutant): a lease
/// holding far more swap than the threshold whose `max` events do not grow (host
/// pressure moved it out while it sat below its cap: never at its cap, or at it once
/// before) runs to its exit. Catches the watch killing on swap that did not rise (the
/// "swap rose dropped" mutant): a lease host pressure swapped out earlier whose `max`
/// events then grow with its swap held runs to its exit. Also catches a kill at the
/// threshold rather than above it, and a lease that booked no memory (no cap) killed
/// for its counters.
#[tokio::test]
async fn swap_without_pressing_the_cap_kills_nothing() {
    let fake = watched("swap-host", 64 << 10, 0);
    let spec = Spec::new(&image(), "unused");
    for (seq, cap) in [(1, Cap::Never), (4, Cap::Once), (5, Cap::HeldThenPressed)] {
        let result = fake
            .run(seq, &spec, &holding(1 << 40, cap))
            .await
            .expect("ran");
        assert_eq!(result.exit_code, 0);
        fake.assert_clean(seq);
    }

    let result = fake
        .run(2, &spec, &holding(64 << 10, Cap::Pressed))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(2);

    fake.action(&holding(1 << 40, Cap::Pressed));
    let action = store_action(&fake.cas, &spec);
    let result = fake
        .runtime
        .run(work(3, action, Resources::new(2000, 0)))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(3);
}

/// Catches a sample that cannot be read taken as a sample of zeros (the "zeros"
/// mutant): the lease hit its cap once (`max` 2) and holds 1 TiB in swap from host
/// pressure; one stretch of samples cannot be read (`memory.swap.current` garbled for
/// about 100 ms), and the samples on either side count the same `max` and swap. Read
/// as zeros, the stretch would reset the previous sample, so the next good one would
/// look like `max` grew and swap rose, and the lease would be killed. Skipped, it
/// changes nothing, and the lease runs to its exit.
#[tokio::test]
async fn an_unreadable_sample_changes_nothing() {
    let fake = watched("swap-unreadable", 64 << 10, 0);
    let swapped = "1099511627776";
    let steps = Steps::default()
        .cgroup("memory.current", &["5000"])
        .cgroup("memory.swap.current", &["0"])
        .cgroup("memory.events", &["max 2", "oom 0", "oom_kill 0"])
        .sleep("0.2")
        .cgroup("memory.swap.current", &[swapped])
        .sleep("0.3")
        .cgroup("memory.swap.current", &["n/a"])
        .sleep("0.1")
        .cgroup("memory.swap.current", &[swapped])
        .sleep("0.4")
        .exit(0);
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), &steps)
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(1);
}

/// Catches the configured share of the booking not reaching the watch: with no floor
/// and 50 %, `Fake::run`'s 1 GiB booking may hold 512 MiB in swap at its cap, and is
/// killed one byte past it.
#[tokio::test]
async fn the_swap_threshold_follows_the_configuration() {
    let fake = watched("swap-share", 0, 50);
    let spec = Spec::new(&image(), "unused");
    let result = fake
        .run(1, &spec, &holding(512 << 20, Cap::Pressed))
        .await
        .expect("ran");
    assert_eq!(result.exit_code, 0);
    fake.assert_clean(1);
    let outcome = fake
        .run(2, &spec, &holding((512 << 20) + 1, Cap::Pressed))
        .await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { limit, .. }) if limit == CAP),
        "{outcome:?}"
    );
    fake.assert_clean(2);
}

/// Catches the swap watch's kill hanging, or reported as anything but the lease's
/// out-of-memory kill, when `cgroup.kill` cannot be written and `crun run`
/// outlives the grace period; and anything of the lease left behind afterwards.
///
/// The test, not the action, writes the lease cgroup's counters, one step after
/// another, so the samples the watch can read do not depend on how fast anything
/// runs: `memory.events` says `max 1` and `memory.swap.current` cannot be read
/// (`n/a`, a sample the watch skips) before the lease cgroup goes read-only, and only
/// then does the swap read 1 MiB. So the first sample the watch reads is its kill
/// sample, whenever it takes it, and its `cgroup.kill` fails. The lease cgroup stays
/// read-only to the end: the clean's own `cgroup.kill` fails too, and `crun delete
/// --force` is what ends the action. (Made writable again by the action after a
/// sleep, the cgroup was still read-only at the clean on a slow runner, and the
/// fake's `delete` left its counters behind: "lease cgroup left".)
#[tokio::test]
async fn the_swap_kill_ends_even_when_cgroup_kill_fails() {
    let fake = Fake::with("swap-kill-fails", |config| {
        config.swap_kill = SwapKill {
            floor_bytes: 64 << 10,
            percent: 0,
            every: Duration::from_millis(20),
        };
    });
    let cgroup = fake.lease_cgroup(1);
    let write = |file: &str, text: &str| std::fs::write(cgroup.join(file), text).expect(file);
    let steps = async {
        // The driver makes the lease cgroup before `crun run` publishes the pid.
        fake.wait_for_start().await;
        write("memory.current", "5000\n");
        write("memory.events", "max 1\noom 0\noom_kill 0\n");
        write("memory.swap.current", "n/a\n");
        std::fs::set_permissions(&cgroup, std::fs::Permissions::from_mode(0o555))
            .expect("make the lease cgroup read-only");
        write("memory.swap.current", "1048576\n");
    };
    let spec = Spec::new(&image(), "unused");
    let started = Instant::now();
    let (outcome, ()) = tokio::join!(fake.run(1, &spec, &["sleep", "30"]), steps);
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { limit, .. }) if limit == CAP),
        "{outcome:?}"
    );
    // The kill waits out the grace, not the action's 30 s: a kill that waits for
    // `crun run` without a bound returns only once the action has ended.
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    assert!(
        !fake
            .events()
            .iter()
            .any(|e| e == "cgroup.kill ended the action"),
        "the premise: cgroup.kill could not be written: {:?}",
        fake.events()
    );
    fake.assert_clean(1);
}

/// Catches a kernel OOM kill at the lease's own cap reported with the swap watch's
/// numbers (RAM plus swap at a sample) rather than `memory.peak`, and the watch
/// killing a lease the host swapped for what that lease then pushes at its cap below
/// the threshold. The samples are the lease's in an arm run of
/// `swapped_by_the_host_then_at_its_cap_a_lease_is_not_killed` (a 192 MiB floor): host
/// reclaim moved 315 MB to swap with `max` 0, then at its cap the lease pushed 68 MB
/// more, and the kernel's OOM killer ran at that cap (`oom 1`, `oom_kill 13`). The
/// run failed with `OutOfMemory { used: 454856704, .. }`, the lease's `memory.peak`:
/// the kernel's kill, read from the lease's counters, not the watch's.
#[tokio::test]
async fn a_kernel_oom_at_the_cap_after_host_swap_is_the_kernels_not_the_watchs() {
    let fake = watched("swap-kernel-oom", 192 << 20, 0);
    // Each sample: memory.current, memory.swap.current, memory.events `max`.
    let samples = [
        ("1282048", "315006976", 0),
        ("1282048", "315006976", 0),
        ("52322304", "314839040", 0),
        ("68202496", "314834944", 491),
        ("68100096", "314834944", 929),
        ("68370432", "314953728", 1198),
        ("68239360", "314953728", 1514),
        ("68300800", "314839040", 1838),
        ("17182720", "314839040", 2101),
        ("19279872", "314839040", 2101),
        ("36839424", "314839040", 2101),
        ("52568064", "314839040", 2101),
        ("53874688", "314839040", 2101),
        ("68390912", "381247488", 2106),
        ("68022272", "382926848", 2129),
    ];
    let mut steps = Steps::default();
    for (current, swap, max) in samples {
        steps = steps
            .cgroup("memory.current", &[current])
            .cgroup("memory.swap.current", &[swap])
            .events(&[
                "low 0",
                "high 0",
                format!("max {max}").as_str(),
                "oom 0",
                "oom_kill 0",
                "oom_group_kill 0",
            ])
            .sleep("0.06");
    }
    let steps = steps
        .events(&[
            "low 0",
            "high 0",
            "max 2129",
            "oom 1",
            "oom_kill 13",
            "oom_group_kill 1",
        ])
        .cgroup("memory.swap.current", &["66035712"])
        .cgroup("memory.peak", &["454856704"])
        .exit(137);
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), &steps).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 454_856_704, limit })
            if limit == CAP),
        "{outcome:?}"
    );
    assert!(
        !fake
            .events()
            .iter()
            .any(|e| e == "cgroup.kill ended the action"),
        "the watch killed the lease: {:?}",
        fake.events()
    );
    fake.assert_clean(1);
}
