//! How the driver reports a lease killed for memory, against the stand-in `podman`
//! (`fixtures/fake-podman.sh`) and a plain directory standing in for the cgroup mount:
//! the action script writes the lease cgroup's `memory.events`, `memory.current` and
//! `memory.swap.current` as the kernel would. `podman_memory.rs` runs the same promises
//! against a real kernel.
//!
//! A module of `fake_podman.rs`, not a test binary of its own. The coverage job counts
//! the lines of the driver's generic `PodmanRuntime` per build of it; split across two
//! test binaries, the swap watch's paths (run here) and the timeout and kill paths (run
//! in `fake_podman.rs`) each counted as missed in the other binary's build.

use std::time::{Duration, Instant};

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_container::SwapKill;
use kbf_types::Resources;

use crate::support::fake::{Fake, image};
use crate::support::{Spec, store_action, work};

/// The cap `Fake::run`'s 1 GiB booking gets: 1.5 GiB + 512 MiB.
const CAP: u64 = (3 << 29) + (512 << 20);

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
    let oom = r#"printf 'low 0\nhigh 4\nmax 1\noom 1\noom_kill 1\n' > "$CG/memory.events"
        echo 2000000000 > "$CG/memory.peak"; exit 137"#;
    let outcome = fake.run(1, &spec, oom).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 2_000_000_000, limit }) if limit == cap),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    // Killed by an ancestor's OOM: `oom_kill` in the lease, its own `oom` never.
    let busy = r#"printf 'max 0\noom 0\noom_kill 3\n' > "$CG/memory.events"; exit 137"#;
    let outcome = fake.run(2, &spec, busy).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why)) if why.contains("killed 3 process")),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    let own = r#"printf 'max 0\noom 0\noom_kill 0\n' > "$CG/memory.events"; exit 137"#;
    let result = fake.run(3, &spec, own).await.expect("ran");
    assert_eq!(result.exit_code, 137);
    fake.assert_clean(3);

    // No memory.events to read: the driver cannot tell, so it does not guess.
    let outcome = fake.run(4, &spec, "exit 137").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("memory.events")),
        "{outcome:?}"
    );
    fake.assert_clean(4);

    // Nor when memory.events carries no count.
    let garbled = r#"printf 'max 0\noom 0\noom_kill many\n' > "$CG/memory.events"; exit 137"#;
    let outcome = fake.run(5, &spec, garbled).await;
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
    let busy = r#"printf 'max 0\noom 0\noom_kill 1\n' > "$CG/memory.events""#;

    std::fs::write(&local, "low 0\noom 4\noom_kill 9\n").expect("write");
    let grew = format!(r#"{busy}; printf 'oom 6\n' > "$CG/../memory.events.local"; exit 137"#);
    let outcome = fake.run(1, &spec, &grew).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("ran 2 time(s) for the actions/ cgroup's own limit")),
        "{outcome:?}"
    );
    fake.assert_clean(1);

    let outcome = fake.run(2, &spec, &format!("{busy}; exit 137")).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("did not run for the actions/ cgroup's own limit")),
        "{outcome:?}"
    );
    fake.assert_clean(2);

    std::fs::write(&local, "garbled\n").expect("write");
    let outcome = fake.run(3, &spec, &format!("{busy}; exit 137")).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why))
            if why.contains("memory.events.local could not be read")),
        "{outcome:?}"
    );
    fake.assert_clean(3);

    let cap = (3u64 << 29) + (512 << 20);
    let own = r#"printf 'max 2\noom 1\noom_kill 1\n' > "$CG/memory.events"
        echo n/a > "$CG/memory.peak"; exit 137"#;
    let outcome = fake.run(4, &spec, own).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used, limit }) if used == cap && limit == cap),
        "{outcome:?}"
    );
    fake.assert_clean(4);
}

/// Catches the lease cgroup's counters read only on exit 137: when Podman recorded no
/// exit (`NotRun`) or could not be asked (`inspect` fails), a kill the counters show is
/// still reported as the memory kill it was, by whose limit, and not as Podman's
/// failure. Without a kill in the counters, what Podman said is reported.
#[tokio::test]
async fn a_memory_kill_without_an_exit_from_podman_is_still_reported() {
    let fake = Fake::new("oom-no-exit");
    let spec = Spec::new(&image(), "unused");
    let own = r#"printf 'max 2\noom 1\noom_kill 1\n' > "$CG/memory.events"
        echo 5000000000 > "$CG/memory.peak"; exit 137"#;
    let busy = r#"printf 'max 0\noom 0\noom_kill 2\n' > "$CG/memory.events"; exit 137"#;

    fake.knob("status-override", "stopped 0\n");
    let outcome = fake.run(1, &spec, own).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { used: 5_000_000_000, limit }) if limit == CAP),
        "{outcome:?}"
    );
    fake.assert_clean(1);
    let outcome = fake.run(2, &spec, busy).await;
    assert!(
        matches!(outcome, Err(RuntimeError::BusyNode(ref why)) if why.contains("killed 2 process")),
        "{outcome:?}"
    );
    fake.assert_clean(2);
    // No kill in the counters: Podman's word stands.
    let outcome = fake.run(3, &spec, "exit 137").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("did not run to an exit")),
        "{outcome:?}"
    );
    fake.assert_clean(3);
    std::fs::remove_file(fake.state.join("status-override")).expect("unset");

    fake.knob("inspect-fails", "");
    let outcome = fake.run(4, &spec, own).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { limit, .. }) if limit == CAP),
        "{outcome:?}"
    );
    let outcome = fake.run(5, &spec, "exit 137").await;
    assert!(
        matches!(outcome, Err(RuntimeError::Failed(ref why)) if why.contains("inspect")),
        "{outcome:?}"
    );
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
fn holding(swap: u64, cap: Cap) -> String {
    // The step from which `max` grows, and the step at which the swap goes out.
    let (start, grow_from, swap_at) = match cap {
        Cap::Pressed => (2, 0, 0),
        Cap::Never => (0, 1000, 0),
        Cap::Once => (2, 1000, 20),
        Cap::HeldThenPressed => (2, 40, 20),
    };
    let first = if swap_at == 0 { swap } else { 0 };
    format!(
        r#"echo 5000 > "$CG/memory.current"; echo {first} > "$CG/memory.swap.current"
        i={start}; n=0
        while [ $n -lt 100 ]; do
            [ $n -eq {swap_at} ] && echo {swap} > "$CG/memory.swap.current"
            printf 'max %d\noom 0\noom_kill 0\n' $i > "$CG/memory.events.new"
            mv "$CG/memory.events.new" "$CG/memory.events"
            [ $n -ge {grow_from} ] && i=$((i + 1)); n=$((n + 1)); sleep 0.01
        done
        exit 0"#
    )
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

    fake.knob("action.sh", &holding(1 << 40, Cap::Pressed));
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
    let script = r#"echo 5000 > "$CG/memory.current"; echo 0 > "$CG/memory.swap.current"
        printf 'max 2\noom 0\noom_kill 0\n' > "$CG/memory.events"
        sleep 0.2; echo 1099511627776 > "$CG/memory.swap.current"
        sleep 0.3; echo n/a > "$CG/memory.swap.current"
        sleep 0.1; echo 1099511627776 > "$CG/memory.swap.current"
        sleep 0.4; exit 0"#;
    let result = fake
        .run(1, &Spec::new(&image(), "unused"), script)
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
/// out-of-memory kill, when `cgroup.kill` cannot be written (the lease cgroup is
/// read-only for a moment) and `podman start` outlives the grace period; and anything
/// of the lease left behind afterwards.
#[tokio::test]
async fn the_swap_kill_ends_even_when_cgroup_kill_fails() {
    let fake = Fake::with("swap-kill-fails", |config| {
        config.swap_kill = SwapKill {
            floor_bytes: 64 << 10,
            percent: 0,
            every: Duration::from_millis(20),
        };
        config.kill_grace = Duration::from_secs(1);
    });
    // The lease cgroup goes read-only before its `max` events start to grow, and
    // writable again 300 ms later, after the watch's kill and before the grace ends.
    let script = r#"echo 5000 > "$CG/memory.current"; echo 1048576 > "$CG/memory.swap.current"
        printf 'max 0\noom 0\noom_kill 0\n' > "$CG/memory.events"
        chmod a-w "$CG"
        for i in 1 2 3 4 5 6 7 8 9 10; do
            printf 'max %d\noom 0\noom_kill 0\n' $i > "$CG/memory.events"; sleep 0.01
        done
        sleep 0.2; chmod u+w "$CG"; exec sleep 30"#;
    let outcome = fake.run(1, &Spec::new(&image(), "unused"), script).await;
    assert!(
        matches!(outcome, Err(RuntimeError::OutOfMemory { limit, .. }) if limit == CAP),
        "{outcome:?}"
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
