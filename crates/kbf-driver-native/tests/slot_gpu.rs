//! The GPU under the slot profile: a pool user, under the exact profile the driver
//! generates for a lease, gets a Metal device, compiles a kernel from source at run time
//! and runs it on the GPU (`slot_gpu/probe.rs`, this binary started as the pool user), and
//! a profile with one of the two denies that took the GPU away on a hosted runner gets
//! none of that, so the probe is seen to fail:
//! - `(deny iokit-open)`: no Metal device (the device step fails);
//! - a `mach-lookup` deny of `com.apple.MTLCompilerService`: the device and queue work and
//!   the kernel cannot be compiled or turned into a pipeline (the library or pipeline step
//!   fails).
//!
//! A GPU lease cleans up like any other: what the run left in the user's cache and
//! temporary folders and home is gone when the slot is given back.
//!
//! `tests/gpu_profile.rs` checks the same two denies against the profile text on every
//! platform; this file shows on a Mac what they do and that the shipped profile has
//! neither. The hosted macOS runner is a virtual machine with a paravirtual GPU (Metal
//! names its device `Apple Paravirtual device`), not a farm Mac: a pass there shows the
//! profile leaves the Metal path open for a pool user with no GUI session, and says
//! nothing of an M-series GPU's throughput, its Metal family or its user clients. The
//! same test is run by hand on a farm Mac (`cargo xtask sudo-test`, below) to see its real
//! GPU; no workflow runs it there.
//!
//! Every test is `#[ignore]`: each needs root and real users. The macOS CI jobs run them
//! with `cargo xtask sudo-test -p kbf-driver-native --test slot_gpu -- --test-threads=1
//! --nocapture`. The probe prints `STAGE` lines, which the tests print again as
//! `GPU-STAGE` lines for the log.

#![cfg(target_os = "macos")]

#[path = "slot_gpu/probe.rs"]
mod probe;
mod support;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use kbf_driver_native::NativeRuntime;
use kbf_driver_native::network::{Isolation, Network, WeakenedProfile};
use kbf_driver_native::slots::macos::Slots;
use kbf_driver_native::slots::{LEASES_ROOT_MODE, Pool};
use support::pool::{Scratch, Users, config, home_of};
use support::{MemoryCas, Spec, stdout};

// The pool user's process is this binary again, started with the probe's argument: it
// answers before the test harness reads its arguments, as the other hooks of `support` do.
const _: () = {
    extern "C" fn gpu_probe_hook() {
        let probe_mode = std::env::args_os().nth(1).is_some_and(|a| a == probe::ARG);
        if probe_mode {
            std::process::exit(probe::run());
        }
    }
    #[used]
    #[unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")]
    static GPU_PROBE_HOOK: extern "C" fn() = gpu_probe_hook;
};

/// The two denies that took the GPU away on the hosted runner, with the steps whose
/// failure shows each.
const PLANTED: [(&str, &str, &[&str]); 2] = [
    ("iokit-open", "(deny iokit-open)", &["device"]),
    (
        "MTLCompilerService",
        "(deny mach-lookup (global-name \"com.apple.MTLCompilerService\"))",
        &["library", "pipeline"],
    ),
];

fn assert_root() {
    support::pool::assert_root("slot_gpu");
}

/// The `STAGE` lines of the probe's output, as `(step, ok)`.
fn stages(output: &str) -> Vec<(String, bool)> {
    output
        .lines()
        .filter(|l| l.starts_with("STAGE "))
        .inspect(|l| println!("GPU-{l}"))
        .filter_map(|l| {
            let mut words = l.split_whitespace().skip(1);
            let step = words.next()?.to_owned();
            Some((step, words.next() == Some("ok")))
        })
        .collect()
}

/// Catches a slot profile that takes the GPU from a pool user, and a driver that cannot
/// run a GPU lease as one: the action is the probe run through the driver as a pool
/// user, with the slot profile as the driver generates it. Every step passes and the
/// process exits 0. (The device step also fails when the user has no access to the GPU
/// for another reason, such as a session that cannot start the Metal services.)
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_pool_user_under_the_slot_profile_runs_a_compute_kernel() {
    assert_root();
    let scratch = Scratch::new("gpu-real");
    let users = Users::new("gpu-real", 1);
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let spec =
        Spec::argv(&[&scratch.as_user_binary(), probe::ARG]).timeout(Duration::from_secs(300));
    let result = support::run(&rt, &cas, 1, &spec).await.expect("ran");
    let out = stdout(&cas, &result);
    println!("{out}");
    let steps = stages(&out);
    let names: Vec<&str> = steps.iter().map(|(s, _)| s.as_str()).collect();
    let want: Vec<&str> = probe::EXIT.iter().map(|(s, _)| *s).collect();
    assert_eq!(names, want, "{out}");
    assert!(steps.iter().all(|(_, ok)| *ok), "{out}");
    assert_eq!(result.exit_code, 0, "{out}");
    assert!(out.contains("GPU-PROBE pass"), "{out}");
}

/// Catches a GPU lease that leaves state for the next lease of its slot: whatever the
/// Metal run wrote under the user's cache and temporary folders (the probe lists both
/// as `CACHE-DIR` lines, so the log says how much there was) and in its home is gone
/// when the lease has ended and the slot is given back. The clean empties those places
/// for every lease; this shows a GPU run does not put anything where it does not. (On the
/// hosted runner the run left three entries in the cache folder and one in the temporary
/// folder, so the check is not vacuous there.)
/// A directory the system placed there (Metal's own cache folders carry `sunlnk`) is kept
/// by the clean, emptied, so the folders must hold no file or link at any depth, not no name.
/// The test covers the user's home, cache and temporary folders only: not a Metal process
/// left running after the lease (the kill at the lease's end is `slot_term`'s), nor a
/// shader cache kept outside those folders.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn a_gpu_lease_leaves_nothing_in_the_slot_users_folders() {
    assert_root();
    let scratch = Scratch::new("gpu-clean");
    let users = Users::new("gpu-clean", 1);
    let home = home_of(&users.0[0]);
    let held = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .map(|d| {
                d.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    };
    let cas = Arc::new(MemoryCas::default());
    let rt = NativeRuntime::new(config(&scratch, users.pool()), Arc::clone(&cas)).expect("runtime");
    let home_before = held(&home);
    let spec =
        Spec::argv(&[&scratch.as_user_binary(), probe::ARG]).timeout(Duration::from_secs(300));
    let result = support::run(&rt, &cas, 1, &spec).await.expect("ran");
    let out = stdout(&cas, &result);
    println!("{out}");
    assert_eq!(result.exit_code, 0, "{out}");
    let folders: Vec<PathBuf> = out
        .lines()
        .filter(|l| l.starts_with("CACHE-DIR "))
        .inspect(|l| println!("GPU-{l}"))
        .filter_map(|l| l.split_whitespace().nth(2).map(PathBuf::from))
        .collect();
    assert_eq!(folders.len(), 2, "the cache and temporary folders: {out}");
    // Every file, link and other non-directory below `dir`: what the clean must remove.
    fn files_below(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(meta) if meta.is_dir() => found.extend(files_below(&path)),
                _ => found.push(path),
            }
        }
        found
    }
    support::pool::wait_until("the slot's folders hold no file again", || {
        folders.iter().all(|f| files_below(f).is_empty())
    });
    assert_eq!(held(&home), home_before, "the home");
}

/// Runs the probe as the pool user of a one-user pool whose slot profile is changed by
/// `change`, through `Slot::launch`, as the driver starts an action: returns the exit
/// code and the probe's output.
async fn probe_under(name: &str, change: WeakenedProfile) -> (Option<i32>, String) {
    assert_root();
    let users = Users::new(name, 1);
    let scratch = Scratch::new(name);
    let pool = Pool::new(users.0.clone()).expect("pool");
    let slots =
        Slots::start_with_profile_change(&pool, Duration::from_secs(10), change).expect("slots");
    let leases = scratch.leases();
    std::fs::create_dir_all(&leases).expect("leases");
    std::fs::set_permissions(&leases, std::fs::Permissions::from_mode(LEASES_ROOT_MODE))
        .expect("chmod");
    let dir = leases.join("lease-1-1");
    std::fs::create_dir(&dir).expect("lease dir");
    let slot = slots.take().await.expect("a slot");
    let lease = std::fs::canonicalize(&dir).expect("real path");
    std::fs::create_dir(lease.join("tmp")).expect("tmp");
    slot.hand_over(&dir).await.expect("hand over");
    let (program, args) = slot
        .launch(
            &Isolation::detect(),
            Network::On,
            &lease,
            PathBuf::from(scratch.as_user_binary()),
            &[probe::ARG.to_owned()],
        )
        .expect("launch");
    let home = home_of(&users.0[0]);
    let output = Command::new(program)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("TMPDIR", lease.join("tmp"))
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .expect("the probe ran");
    slot.release().await;
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

/// Catches a probe that passes whatever the profile says (a device found by another
/// route, a step that never runs the kernel, an exit code that is always 0): each deny
/// added to the end of the slot profile the driver generates makes the probe fail at
/// the step that needs what the deny removed, with that step's exit code, and no step
/// after it runs. Seen on the hosted runner (a paravirtual GPU) before this test: the
/// `iokit-open` deny leaves no device, and the `MTLCompilerService` deny leaves the device
/// and queue and breaks the pipeline.
#[ignore = "needs root and real users: cargo xtask sudo-test"]
#[tokio::test(flavor = "multi_thread")]
async fn each_deny_that_takes_the_gpu_fails_the_probe_at_its_step() {
    for (label, deny, failing) in PLANTED {
        let (code, out) = probe_under(
            &format!("gpu-{}", label.to_ascii_lowercase()),
            WeakenedProfile::Add(deny),
        )
        .await;
        println!("{out}");
        let steps = stages(&out);
        let (last, ok) = steps.last().expect("the probe printed a step");
        assert!(!ok, "{label}: the last step passed\n{out}");
        assert!(
            failing.contains(&last.as_str()),
            "{label}: failed at {last}\n{out}"
        );
        let want = probe::EXIT.iter().find(|(s, _)| s == last).map(|(_, c)| *c);
        assert_eq!(code, want, "{label}\n{out}");
        assert!(!out.contains("GPU-PROBE pass"), "{label}\n{out}");
    }
}
