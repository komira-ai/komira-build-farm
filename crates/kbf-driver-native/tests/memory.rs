//! The memory watch: an action whose processes together hold more than the lease's
//! limit is killed, whole, and reported out of memory; one within it runs to the end.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::MemoryPolicy;
use support::{MemoryCas, PROMPT, Spec, alive, config, no_leases, pid_in, runtime, scratch, work};

/// Perl that writes its pid to `$PIDFILE`, then fills and holds `$MIB` MiB and
/// sleeps.
const HOG: &str = r#"
open(my $f, '>', $ENV{PIDFILE}) or die; print $f "$$\n"; close($f);
my $x = "a" x ($ENV{MIB} * 1024 * 1024);
sleep 300;
"#;

/// Catches: the memory watch skipped (the hog runs to its timeout), memory counted
/// for the leader only (the hog is a grandchild), the hog left alive after the kill,
/// and an out-of-memory end reported as anything but OutOfMemory with the limit.
#[tokio::test]
async fn an_action_past_its_limit_is_killed_whole() {
    let dir = scratch("oom");
    let mut config = config(&dir);
    config.memory = MemoryPolicy {
        percent: 100,
        headroom_bytes: 0,
    };
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let pidfile = dir.join("pid");
    let action = Spec::sh("/usr/bin/perl -e \"$HOG\" & wait")
        .env("PIDFILE", pidfile.to_str().expect("utf8"))
        .env("HOG", HOG)
        .env("MIB", "256")
        .timeout(Duration::from_secs(60))
        .store(&cas);
    let limit = 64 << 20;
    let outcome = tokio::time::timeout(PROMPT * 3, rt.run(work(1, action, limit)))
        .await
        .expect("the watch ends the run long before its timeout");
    let pid = pid_in(&pidfile).await;
    match outcome {
        Err(RuntimeError::OutOfMemory { used, limit: l }) => {
            assert_eq!(l, limit);
            assert!(used > limit, "{used} <= {limit}");
        }
        other => panic!("not out of memory: {other:?}"),
    }
    assert!(!alive(pid), "the hog {pid} outlived the kill");
    assert!(no_leases(&config));
}

/// Catches: a limit applied to an action within it (killed for memory it does not
/// hold), or a limit applied when nothing was booked.
#[tokio::test]
async fn an_action_within_its_limit_runs_to_the_end() {
    let dir = scratch("within");
    let config = config(&dir);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config, &cas);
    let pidfile = dir.join("pid");
    let script = "/usr/bin/perl -e 'open(my $f, \">\", $ENV{PIDFILE}); print $f \"$$\\n\"; close($f); my $x = \"a\" x (32*1024*1024); select(undef,undef,undef,0.3)'";
    for (seq, booked) in [(1, 64_u64 << 20), (2, 0)] {
        let action = Spec::sh(script)
            .env("PIDFILE", pidfile.to_str().expect("utf8"))
            .store(&cas);
        let result = rt.run(work(seq, action, booked)).await.expect("ran");
        assert_eq!(result.exit_code, 0, "booked {booked}");
    }
}
