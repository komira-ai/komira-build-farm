//! A simulator of the lease's own (`simulator=<device type>,<runtime>`,
//! `kbf_driver_native::simulator`): made in the default device set under the lease's
//! name before the action runs, named to it, and shut down and deleted when the lease
//! ends whatever the outcome, or at the next start when the daemon was killed.
//!
//! Everywhere, a fake `xcrun` (kbf-testkit's, through a link to this test binary)
//! stands in for `xcrun simctl` and logs every call; the tests read that log. Its
//! `simctl list devices -j` prints the file `devices.json` beside it, which a test
//! writes. On macOS the `#[ignore]`d tests at the end make, boot and delete real
//! simulators; the native-macos job runs them with `--ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::support::{
    MemoryCas, PROMPT, Spec, config, no_leases, run, runtime, scratch, stderr, stdout,
};
use kbf_daemon::{Runtime, RuntimeError};
use kbf_driver_native::NativeConfig;
use kbf_driver_native::network::Isolation;
use kbf_driver_native::simulator::{Listing, MARKER, UDID_VAR};

/// The UDID the fake `simctl create` prints for [`DEVICE_TYPE`].
const UDID: &str = "0A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// A second device's UDID: one named for the same lease (a create cut short made it
/// late).
const OTHER: &str = "1A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// The UDID the fake prints for [`STUCK_TYPE`], which its `delete` refuses.
const STUCK: &str = "2A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// A device of the shared set no lease made.
const SHARED: &str = "3A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";
/// Another lease's device.
const NEIGHBOUR: &str = "4A1B2C3D-4E5F-4071-8293-A4B5C6D7E8F9";

const DEVICE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.iPhone-16";
const RUNTIME: &str = "com.apple.CoreSimulator.SimRuntime.iOS-18-4";
/// A device type the fake `create` refuses, as `simctl` does one it does not know.
const BAD_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Nothing";
/// A device type whose device the fake `delete` refuses.
const STUCK_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Stuck";
/// A device type for which the fake `create` prints no UDID.
const MUTE_TYPE: &str = "com.apple.CoreSimulator.SimDeviceType.Mute";

/// What the real `xcrun xcdevice list` prints for one simulator, `available` or not.
fn xcdevice_entry(udid: &str, available: bool) -> String {
    format!(
        r#"{{"simulator": true, "available": {available}, "identifier": "{udid}", "name": "kbf-lease-1-1"}}"#
    )
}

/// A fake `xcrun` at `dir/bin/xcrun` with `extra` rules before the default ones:
/// `simctl create` prints the device's UDID (none for [`MUTE_TYPE`]); [`BAD_TYPE`] is
/// refused with exit 148; `list devices -j` prints `dir/bin/devices.json` (no devices
/// until a test writes one); `shutdown` exits 149, as `simctl` does for a device that
/// is not booted (the driver must not care); `delete` of [`STUCK`] fails, every other
/// `delete` succeeds; `xcdevice list` and `xcodebuild -showdestinations` (as the driver asks, [`SHOW`])
/// list every device the fake `create` makes ([`UDID`] and [`STUCK`]), so Xcode lists a
/// lease's device at once.
fn fake_xcrun(dir: &Path, extra: &str) -> PathBuf {
    let bin = dir.join("bin");
    std::fs::create_dir_all(&bin).expect("bin");
    let xcrun = bin.join("xcrun");
    kbf_testkit::act::link_self(&xcrun).expect("link");
    let list = bin.join("devices.json");
    std::fs::write(&list, br#"{"devices": {}}"#).expect("list");
    let list = list.display();
    let shown = bin.join("xcdevice.json");
    let entries = [xcdevice_entry(UDID, true), xcdevice_entry(STUCK, true)];
    std::fs::write(&shown, format!("[{}]", entries.join(", "))).expect("xcdevice");
    let shown = shown.display();
    let shown_by_xcodebuild = bin.join("showdestinations.txt");
    std::fs::write(&shown_by_xcodebuild, showing(&[UDID, STUCK], &[])).expect("showdestinations");
    let shown_by_xcodebuild = shown_by_xcodebuild.display();
    let rules = format!(
        "{extra}\
         xcdevice list --timeout 5\t-\tcat {shown}\n\
         {SHOW}\t-\tcat {shown_by_xcodebuild}\n\
         simctl bootstatus * -b\t-\texit 0\n\
         simctl create * {BAD_TYPE} *\t-\techo --stderr Invalid device type: $4 ; exit 148\n\
         simctl create * {MUTE_TYPE} *\t-\techo\n\
         simctl create * {STUCK_TYPE} *\t-\techo {STUCK}\n\
         simctl create kbf-lease-* {DEVICE_TYPE} {RUNTIME}\t-\techo {UDID}\n\
         simctl list devices -j\t-\tcat {list}\n\
         simctl shutdown *\t-\techo --stderr Unable to shutdown device in current state: Shutdown ; exit 149\n\
         simctl delete {STUCK}\t-\techo --stderr Unable to delete ; exit 1\n\
         simctl delete *\t-\texit 0\n"
    );
    std::fs::write(format!("{}.rules", xcrun.display()), rules).expect("rules");
    xcrun
}

/// Makes the fake `xcrun`'s `simctl list devices -j` print `devices` (name, UDID),
/// all under one runtime.
fn list(xcrun: &Path, devices: &[(&str, &str)]) {
    let devices: Vec<String> = (devices.iter())
        .map(|(name, udid)| {
            format!(r#"{{"name": "{name}", "udid": "{udid}", "state": "Shutdown"}}"#)
        })
        .collect();
    let json = format!(
        r#"{{"devices": {{"{RUNTIME}": [{}]}}}}"#,
        devices.join(", ")
    );
    std::fs::write(xcrun.with_file_name("devices.json"), json).expect("list");
}

/// What the fake `xcrun` was asked of `simctl`, one call a line, `xcrun simctl` left
/// out: `create kbf-lease-1-1 <type> <runtime>`, `delete <udid>`, ...; the
/// `xcdevice list` looks are counted by [`looks`].
fn calls(xcrun: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(format!("{}.log", xcrun.display())).unwrap_or_default();
    let prefix = format!("{} simctl ", xcrun.display());
    let look = format!("{} {LOOK}", xcrun.display());
    log.lines()
        .filter(|line| *line != look && !line.contains(SHOWING))
        .map(|line| {
            line.strip_prefix(&prefix)
                .unwrap_or_else(|| panic!("{line:?} is not a simctl call"))
                .to_owned()
        })
        .collect()
}

/// How the driver asks Xcode for its devices: `xcrun` and this.
const LOOK: &str = "xcdevice list --timeout 5";

/// What the driver's second look runs, in the fake's argument pattern: the action's own
/// `xcodebuild -showdestinations`, on the probe project the driver makes in the lease
/// directory.
const SHOW: &str = "xcodebuild -showdestinations -project * -scheme IOSFixture -derivedDataPath *";

/// What a call of [`SHOW`] has in the fake's log, whatever its last argument.
const SHOWING: &str = " xcodebuild -showdestinations ";

/// How many times the fake `xcrun` was asked [`SHOW`] in `log`.
fn shows_in(log: &str) -> usize {
    log.lines().filter(|line| line.contains(SHOWING)).count()
}

/// What `xcodebuild -showdestinations` prints when it lists `simulators` (UDIDs) as
/// eligible destinations, and `ineligible` under the heading that follows them: the
/// placeholders every run prints, and no simulator at all when both are empty.
fn showing(simulators: &[&str], ineligible: &[&str]) -> String {
    let line = |udid: &&str| {
        format!(
            "\t\t{{ platform:iOS Simulator, arch:arm64, id:{udid}, OS:18.4, name:iPhone 16 }}\n"
        )
    };
    format!(
        "Command line invocation:\n    xcodebuild -showdestinations -scheme IOSFixture\n\n\
         Available destinations for the \"IOSFixture\" scheme:\n\
         \t\t{{ platform:macOS, arch:arm64, id:00006000-000A1B2C3D4E5F6A, name:My Mac }}\n\
         \t\t{{ platform:iOS Simulator, id:dvtdevice-DVTiOSDeviceSimulatorPlaceholder-iphonesimulator:placeholder, name:Any iOS Simulator Device }}\n\
         {}\n\tIneligible destinations for the \"IOSFixture\" scheme:\n{}",
        simulators.iter().map(line).collect::<String>(),
        ineligible.iter().map(line).collect::<String>(),
    )
}

/// How many times the fake `xcrun` was asked `xcdevice list` (with [`LOOK`]'s
/// arguments) in `log`, what it wrote to `<xcrun>.log`.
fn looks_in(log: &str) -> usize {
    log.lines()
        .filter(|line| line.ends_with(&format!("/xcrun {LOOK}")))
        .count()
}

/// How many times the fake `xcrun` was asked `xcdevice list` so far.
fn looks(xcrun: &Path) -> usize {
    looks_in(&std::fs::read_to_string(format!("{}.log", xcrun.display())).unwrap_or_default())
}

/// Forgets the calls so far.
fn new_log(xcrun: &Path) {
    let _ = std::fs::remove_file(format!("{}.log", xcrun.display()));
}

/// The calls one lease that made [`UDID`] makes, when `also` are the other devices
/// named for it at the end: the create, the boot the driver waits on, and the end.
fn lease_calls(seq: u64, also: &[&str]) -> Vec<String> {
    let mut want = vec![
        format!("create kbf-lease-1-{seq} {DEVICE_TYPE} {RUNTIME}"),
        format!("bootstatus {UDID} -b"),
        "list devices -j".to_owned(),
    ];
    for udid in std::iter::once(&UDID).chain(also) {
        want.push(format!("shutdown {udid}"));
        want.push(format!("delete {udid}"));
    }
    want
}

/// A configuration with the fake `xcrun` and no sandbox (on macOS too: the fake writes
/// its log outside the lease directory, which the real sandbox refuses).
fn fake_config(dir: &Path, extra: &str) -> (NativeConfig, PathBuf) {
    let mut config = config(dir);
    config.isolation = Isolation::None;
    config.xcrun = fake_xcrun(dir, extra);
    let xcrun = config.xcrun.clone();
    (config, xcrun)
}

/// The real path of lease `1.seq`'s directory, once the scratch root exists.
fn lease_of(config: &NativeConfig, seq: u64) -> PathBuf {
    std::fs::canonicalize(&config.scratch)
        .expect("scratch root")
        .join(format!("lease-1-{seq}"))
}

fn asks(spec: Spec, device_type: &str) -> Spec {
    spec.property("simulator", &format!("{device_type},{RUNTIME}"))
}

/// Catches: no simulator made for an action that asks for one, or one not named for
/// the lease; its UDID not named to the action, or the Command's variable winning
/// over the driver's; the device not shut down and deleted at the end; a device named
/// for the lease that the create did not print (one a create cut short made late) left
/// behind; a device of the shared set or another lease's deleted (the end matching
/// names by prefix, or deleting every device listed), or the lease's own deleted twice;
/// and `xcrun` run at all for an action that asks for none.
#[tokio::test]
async fn a_lease_that_asks_for_a_simulator_gets_its_own() {
    let dir = scratch("sim-own");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[
            ("iPhone 16", SHARED),
            ("kbf-lease-1-1", UDID),
            ("kbf-lease-1-1", OTHER),
            ("kbf-lease-1-12", NEIGHBOUR),
        ],
    );
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec =
        asks(Spec::act(&format!("env {UDID_VAR}")), DEVICE_TYPE).env(UDID_VAR, "the-commands");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert_eq!(stdout(&cas, &result), format!("{UDID}\n"));
    assert_eq!(calls(&xcrun), lease_calls(1, &[OTHER]));
    assert!(no_leases(&config), "the lease directory is removed");

    let plain = Spec::act(&format!("env {UDID_VAR} none"));
    let result = run(&rt, &cas, 2, &plain).await.expect("ran");
    assert_eq!(stdout(&cas, &result), "none\n");
    assert_eq!(calls(&xcrun).len(), 7, "no simctl call for lease 2");
}

/// Catches the end deleting what the action's lease directory says rather than what
/// the daemon kept: an action that rewrites its directory (the marker made a link to
/// another lease's, a file named for another device) changes nothing the end does.
#[tokio::test]
async fn the_end_deletes_the_device_the_daemon_made_whatever_the_lease_directory_holds() {
    let dir = scratch("sim-dir-edited");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[("kbf-lease-1-12", NEIGHBOUR), ("iPhone 16", SHARED)],
    );
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let lease = lease_of(&config, 1);
    let steps = format!(
        "run rm {marker} ; symlink {other} {marker} ; mkdir {lease}/{SHARED} ; mkdir {lease}/sims/{NEIGHBOUR}",
        marker = lease.join(MARKER).display(),
        other = dir.join("elsewhere").display(),
        lease = lease.display(),
    );
    let result = run(&rt, &cas, 1, &asks(Spec::act(&steps), DEVICE_TYPE)).await;
    assert_eq!(result.expect("ran").exit_code, 0);
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches: the simulator left behind when the action fails (exits non-zero), times
/// out or is killed: the next lease's tests would find a booted, dirty device, and
/// the node would fill with them. Mutant planted: the end deletes simulators only
/// after an action that exited 0 (red on all three).
#[tokio::test]
async fn the_simulator_is_deleted_whatever_the_outcome() {
    let dir = scratch("sim-outcome");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));

    let failed = run(&rt, &cas, 1, &asks(Spec::act("exit 3"), DEVICE_TYPE)).await;
    assert_eq!(failed.expect("ran").exit_code, 3);
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));

    new_log(&xcrun);
    let slow = asks(Spec::act("sleep 300"), DEVICE_TYPE).timeout(Duration::from_millis(300));
    let timed_out = run(&rt, &cas, 2, &slow).await;
    assert!(
        matches!(timed_out, Err(RuntimeError::TimedOut { .. })),
        "{timed_out:?}"
    );
    assert_eq!(calls(&xcrun), lease_calls(2, &[]));

    new_log(&xcrun);
    let started = dir.join("started");
    let steps = format!("append {} x ; sleep 300", started.display());
    let action = asks(Spec::act(&steps), DEVICE_TYPE).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(3, action, 0)).await }
    });
    wait_for(&started).await;
    rt.kill(kbf_types::LeaseId::new(1, 3)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert_eq!(calls(&xcrun), lease_calls(3, &[]));
    assert!(no_leases(&config), "every lease directory is removed");
}

/// Waits until `path` exists.
async fn wait_for(path: &Path) {
    let waited = tokio::time::timeout(PROMPT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("it appears");
}

/// Catches a run the daemon drops (its future dropped mid-action) leaving its
/// simulator behind until the next daemon start: the drop must delete it as the end
/// does. Mutant planted: the drop only removes the directory (red).
#[tokio::test]
async fn a_dropped_run_deletes_its_simulator() {
    let dir = scratch("sim-dropped");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let started = dir.join("started");
    let steps = format!("append {} x ; sleep 300", started.display());
    let action = asks(Spec::act(&steps), DEVICE_TYPE).store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    wait_for(&started).await;
    running.abort();
    let dropped = running.await.expect_err("the run is dropped");
    assert!(dropped.is_cancelled(), "{dropped:?}");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config), "the lease directory is removed");
}

/// Catches: a kill while `simctl create` runs cutting the create short (the device it
/// makes after the end looked would be left), the action run after the kill, and the
/// device that create made left behind.
#[tokio::test]
async fn a_kill_during_the_create_still_deletes_the_simulator() {
    let dir = scratch("sim-kill-create");
    let creating = dir.join("creating");
    let extra = format!(
        "simctl create * slow *\t-\tappend {} x ; sleep 0.5 ; echo {UDID}\n",
        creating.display()
    );
    let (config, xcrun) = fake_config(&dir, &extra);
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let ran = dir.join("ran");
    let action = Spec::act(&format!("append {} x", ran.display()))
        .property("simulator", &format!("slow,{RUNTIME}"))
        .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    wait_for(&creating).await;
    rt.kill(kbf_types::LeaseId::new(1, 1)).await;
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(calls(&xcrun)[1..], lease_calls(1, &[])[2..]);
    assert!(no_leases(&config));
}

/// A configuration as [`fake_config`]'s, whose driver waits up to `within` for Xcode
/// to list a lease's simulator, looking every `every`.
fn listing_config(
    dir: &Path,
    extra: &str,
    within: Duration,
    every: Duration,
) -> (NativeConfig, PathBuf) {
    let (mut config, xcrun) = fake_config(dir, extra);
    config.simulator_listing = Listing {
        boot: PROMPT,
        within,
        every,
        look: Duration::from_secs(30),
    };
    (config, xcrun)
}

/// A rule for the fake `xcrun` that answers the call `key` (an argument pattern) with
/// `listings` in turn, then the last one again and again: each run prints `<name>/0`,
/// then moves each file one place down the queue (`1` to `0`, ...). The kbf-act steps
/// have no branch, so the queue is the order.
fn turns(dir: &Path, name: &str, key: &str, listings: &[String]) -> String {
    let queue = dir.join(name);
    std::fs::create_dir_all(&queue).expect("queue");
    let mut steps = vec![format!("cat {}/0", queue.display())];
    for (n, listing) in listings.iter().enumerate() {
        std::fs::write(queue.join(n.to_string()), listing).expect("listing");
        if n > 0 {
            steps.push(format!("copy {q}/{} {q}/{n}", n - 1, q = queue.display()));
        }
    }
    format!("{key}\t-\t{}\n", steps.join(" ; "))
}

/// [`turns`] for `xcdevice list`.
fn listings_in_turn(dir: &Path, listings: &[String]) -> String {
    turns(dir, "queue", LOOK, listings)
}

/// Catches the action started before Xcode lists the lease's simulator (issue #608:
/// on a hosted Mac, `xcodebuild` listed no simulator at all for up to minutes after
/// the device was made, and an `xcodebuild test -destination id=...` started then
/// fails after its own 30 s): the driver must look again until the device is listed,
/// and only then run the action. The fake's first three listings do not name the
/// device (none at all; another device; the device marked unavailable), the fourth
/// does. The action prints the fake's log, which holds four looks when it starts.
/// Mutants planted: no wait (red: no look, the action sees none); one look only, its
/// answer not read (red: one look); `available: false` taken for listed (red: three
/// looks).
#[tokio::test]
async fn the_action_waits_until_xcode_lists_its_simulator() {
    let dir = scratch("sim-listed-late");
    let queue = listings_in_turn(
        &dir,
        &[
            "[]".to_owned(),
            format!("[{}]", xcdevice_entry(SHARED, true)),
            format!("[{}]", xcdevice_entry(UDID, false)),
            format!(
                "[{}, {}]",
                xcdevice_entry(SHARED, true),
                xcdevice_entry(UDID, true)
            ),
        ],
    );
    let (config, xcrun) = listing_config(&dir, &queue, PROMPT, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let log = format!("{}.log", xcrun.display());
    let result = run(
        &rt,
        &cas,
        1,
        &asks(Spec::act(&format!("cat {log}")), DEVICE_TYPE),
    )
    .await;
    let result = result.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert_eq!(
        looks_in(&stdout(&cas, &result)),
        4,
        "looks before the action ran"
    );
    assert_eq!(looks(&xcrun), 4, "no look after the device was listed");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches a lease whose simulator Xcode never lists running its action anyway (which
/// would fail later, blamed on the action) or failing without saying why: the lease
/// fails as the node's (`Failed`) once the wait is up, naming the device, the wait and
/// the last listing (or how the last look failed), the action not run and the device
/// still deleted. Mutant planted: the wait ends the first look that fails (red: one
/// look, and lease 2's message).
#[tokio::test]
async fn a_simulator_xcode_never_lists_fails_the_lease_saying_so() {
    let dir = scratch("sim-never-listed");
    let other = format!("[{}]", xcdevice_entry(SHARED, true)).replace(' ', "");
    let extra = format!(
        "{LOOK}\tTMPDIR=*-1-2\techo --stderr xcdevice: no devices here ; exit 1\n\
         {LOOK}\t-\techo {other}\n"
    );
    let within = Duration::from_millis(300);
    let (config, xcrun) = listing_config(&dir, &extra, within, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let ran = dir.join("ran");
    let spec = asks(
        Spec::act(&format!("append {} x", ran.display())),
        DEVICE_TYPE,
    );

    let outcome = run(&rt, &cas, 1, &spec).await;
    let Err(RuntimeError::Failed(why)) = &outcome else {
        panic!("{outcome:?}");
    };
    let want = format!("simulator: Xcode did not list {UDID} within {within:?} (");
    assert!(why.starts_with(&want), "{why}");
    assert!(
        why.ends_with(&format!("looks); the last listing: {other}")),
        "{why}"
    );
    assert!(looks(&xcrun) > 1, "looked again");
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));

    new_log(&xcrun);
    let outcome = run(&rt, &cas, 2, &spec).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.contains(&format!("did not list {UDID} within")),
        "{shown}"
    );
    assert!(
        shown.contains(
            "looks); xcdevice list --timeout 5: exit status: 1: xcdevice: no devices here"
        ),
        "{shown}"
    );
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(calls(&xcrun), lease_calls(2, &[]));
    assert!(no_leases(&config));
}

/// Catches a wait that costs a lease whose simulator Xcode lists at once: one look,
/// no pause before it or after it. The pause between looks is a minute here, and the
/// run must end within the harness's 30 s. Mutant planted: a pause before the first
/// look (red: the run times out).
#[tokio::test]
async fn a_simulator_listed_at_once_adds_no_wait() {
    let dir = scratch("sim-listed-at-once");
    let minute = Duration::from_secs(60);
    let (config, xcrun) = listing_config(&dir, "", minute * 2, minute);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let started = std::time::Instant::now();
    let result = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    assert_eq!(result.expect("ran").exit_code, 0);
    assert!(started.elapsed() < minute, "{:?}", started.elapsed());
    assert_eq!(looks(&xcrun), 1);
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
}

/// How many times the fake `xcrun` was asked `simctl bootstatus` in `log`.
fn boots_in(log: &str) -> usize {
    log.lines()
        .filter(|line| line.contains(" simctl bootstatus "))
        .count()
}

/// The fake's log so far.
fn log_of(xcrun: &Path) -> String {
    std::fs::read_to_string(format!("{}.log", xcrun.display())).unwrap_or_default()
}

/// A rule for the fake `xcrun` that answers `key` by running, in turn, the shell
/// scripts in `scripts`, then the last again and again: the call exits as the script
/// does. The kbf-act steps have no branch, so a queue of scripts is the order.
fn scripts_in_turn(dir: &Path, name: &str, key: &str, scripts: &[&str]) -> String {
    let queue = dir.join(name);
    std::fs::create_dir_all(&queue).expect("queue");
    let q = queue.display();
    let mut steps = vec![format!("copy {q}/now {q}/0")];
    for (n, script) in scripts.iter().enumerate() {
        std::fs::write(queue.join(n.to_string()), script).expect("script");
        if n > 0 {
            steps.push(format!("copy {q}/{} {q}/{n}", n - 1));
        }
    }
    steps.push(format!("run sh {q}/now"));
    format!("{key}\t-\t{}\n", steps.join(" ; "))
}

/// The fake's rule for a `bootstatus` that never succeeds, as `simctl` fails a device
/// that will not boot.
const WONT_BOOT: &str =
    "simctl bootstatus * -b\t-\techo --stderr Unable to boot device ; exit 149\n";

/// Catches the driver asking Xcode for its destinations before the device is booted,
/// the state the action asks in (issue #608: the look made on a device not yet
/// booted listed 37 simulators 8 to 19 s in, and the action's own `xcodebuild
/// -showdestinations` after its `bootstatus -b` listed none for 50 to 130 s, so
/// `xcodebuild test -destination` failed in the action). The fake `xcodebuild` lists
/// the device only once a `bootstatus` has run, and the fake `xcdevice` lists it at
/// once: the driver must run the boot first, and the log must show it before every
/// look. Mutants planted: no boot (red: the lease fails, Xcode never lists the
/// device); the boot after the looks (red: the lease fails the same way, the first
/// look found it not listed and the order assertion).
#[tokio::test]
async fn the_simulator_is_booted_before_the_looks_that_ask_xcode_for_it() {
    let dir = scratch("sim-booted-first");
    let (live, booted) = (dir.join("live.txt"), dir.join("booted.txt"));
    std::fs::write(&live, showing(&[SHARED], &[])).expect("live");
    std::fs::write(&booted, showing(&[SHARED, UDID], &[])).expect("booted");
    let extra = format!(
        "simctl bootstatus * -b\t-\tcopy {live} {booted}\n{SHOW}\t-\tcat {live}\n",
        live = live.display(),
        booted = booted.display()
    );
    let (config, xcrun) = listing_config(&dir, &extra, PROMPT, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let result = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let result = result.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    let log = log_of(&xcrun);
    let boot = log.find(" simctl bootstatus ").expect("the boot");
    let look = log.find(SHOWING).expect("a look");
    assert!(boot < look, "the boot after the first look:\n{log}");
    assert_eq!(boots_in(&log), 1, "one boot, not one a look:\n{log}");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches a boot that gives up at the first try (a `bootstatus` can fail, or be cut
/// at [`Listing::look`], while `CoreSimulatorService` is still at it; the next one
/// waits on the boot) and a lease that runs the action before the boot ends: the
/// fake's first two `bootstatus` exit 1, the third exits 0. The action prints the
/// fake's log, which holds three boots and one look when it starts. Mutant planted:
/// the wait ends at the first failed try (red: the lease fails).
#[tokio::test]
async fn the_driver_tries_the_boot_again_and_runs_the_action_once_it_is_booted() {
    let dir = scratch("sim-boot-late");
    let queue = scripts_in_turn(
        &dir,
        "boots",
        "simctl bootstatus * -b",
        &["exit 1", "exit 1", "exit 0"],
    );
    let (config, xcrun) = listing_config(&dir, &queue, PROMPT, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let log = format!("{}.log", xcrun.display());
    let spec = asks(Spec::act(&format!("cat {log}")), DEVICE_TYPE);
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    let seen = stdout(&cas, &result);
    assert_eq!(boots_in(&seen), 3, "boots before the action ran:\n{seen}");
    assert_eq!(shows_in(&seen), 1, "looks before the action ran:\n{seen}");
    assert_eq!(boots_in(&log_of(&xcrun)), 3, "no boot after it was booted");
    assert!(no_leases(&config));
}

/// Catches a device that never boots holding the lease for ever, or failing it
/// without saying why: the lease fails as the node's (`Failed`) once [`Listing::boot`]
/// is up, naming the device, the wait and what the last try printed; no look is made,
/// the action does not run and the device is still deleted. Mutant planted: the
/// failed boot ignored (red: the lease goes on to the looks and runs the action).
#[tokio::test]
async fn a_simulator_that_will_not_boot_fails_the_lease_saying_so() {
    let dir = scratch("sim-never-boots");
    let (mut config, xcrun) = listing_config(&dir, WONT_BOOT, PROMPT, Duration::from_millis(10));
    config.simulator_listing.boot = Duration::from_millis(300);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let ran = dir.join("ran");
    let spec = asks(
        Spec::act(&format!("append {} x", ran.display())),
        DEVICE_TYPE,
    );
    let outcome = run(&rt, &cas, 1, &spec).await;
    let Err(RuntimeError::Failed(why)) = &outcome else {
        panic!("{outcome:?}");
    };
    let want = format!("simulator: {UDID} not booted within 300ms (");
    assert!(why.starts_with(&want), "{why}");
    let last =
        format!("tries); simctl bootstatus {UDID} -b: exit status: 149: Unable to boot device");
    assert!(why.ends_with(&last), "{why}");
    let log = log_of(&xcrun);
    assert!(boots_in(&log) > 1, "tried again:\n{log}");
    assert_eq!(
        looks_in(&log) + shows_in(&log),
        0,
        "no look of a device not booted"
    );
    assert!(!ran.exists(), "the action did not run");
    let calls = calls(&xcrun);
    assert_eq!(calls[0], lease_calls(1, &[])[0]);
    assert_eq!(calls[calls.len() - 3..], lease_calls(1, &[])[2..]);
    assert!(no_leases(&config));
}

/// Catches a kill during the wait for the boot not ending it (it would wait out
/// [`Listing::boot`]) or running the action after it: the run ends `Killed`, the
/// action not run, and the device is deleted.
#[tokio::test]
async fn a_kill_during_the_wait_for_the_boot_ends_it() {
    let dir = scratch("sim-kill-boot");
    let (config, xcrun) = listing_config(&dir, WONT_BOOT, PROMPT, Duration::from_millis(10));
    let mut config = config;
    config.simulator_listing.boot = PROMPT * 6;
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let ran = dir.join("ran");
    let action = asks(
        Spec::act(&format!("append {} x", ran.display())),
        DEVICE_TYPE,
    )
    .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    let waited = tokio::time::timeout(PROMPT, async {
        while boots_in(&log_of(&xcrun)) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("the driver tries the boot again");
    let killed = tokio::time::timeout(PROMPT, rt.kill(kbf_types::LeaseId::new(1, 1)));
    killed.await.expect("the kill ends the wait");
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert!(!ran.exists(), "the action did not run");
    let calls = calls(&xcrun);
    assert_eq!(calls[calls.len() - 3..], lease_calls(1, &[])[2..]);
    assert!(no_leases(&config));
}

/// Catches a `bootstatus` that stalls (a boot with a data migration took up to 2 min
/// 46 s) holding the wait for as long as it likes: each try has [`Listing::look`] to
/// answer (200 ms here) and is killed after it. The fake's `bootstatus` sleeps 5 s.
/// Mutant planted: the try given the `simctl` answer time (red: the run takes the
/// 5 s).
#[tokio::test]
async fn a_boot_that_does_not_answer_is_cut_and_the_wait_still_ends() {
    let dir = scratch("sim-boot-stalls");
    let extra = "simctl bootstatus * -b\t-\tsleep 5\n";
    let (mut config, xcrun) = listing_config(&dir, extra, PROMPT, Duration::from_millis(10));
    config.simulator_listing.boot = Duration::from_millis(500);
    config.simulator_listing.look = Duration::from_millis(200);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let started = std::time::Instant::now();
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let Err(RuntimeError::Failed(why)) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("no answer within 200ms"), "{why}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert!(boots_in(&log_of(&xcrun)) > 1);
    assert!(no_leases(&config));
}

/// Catches the driver's wait standing for another question than the action's (issue
/// #608, runs 38770452545 and 38787596842: `xcrun xcdevice list` named the new device
/// within 10 s, and the action's four sandboxed `xcodebuild -showdestinations` tries
/// that followed each listed no simulator at all, so the lease failed in the action):
/// the driver must look again with `xcodebuild -showdestinations` until it names the
/// device as an eligible destination, and only then run the action. `xcdevice` names
/// the device at once; the fake `xcodebuild`'s first three listings do not (the
/// placeholders only; another simulator; the device under the ineligible heading), the
/// fourth does. The action prints the fake's log, which holds four such looks when it
/// starts. Mutants planted: the look reverted to `xcdevice list` alone (red: no
/// `xcodebuild` look); the ineligible heading not cut (red: three looks).
#[tokio::test]
async fn the_action_waits_until_xcodebuild_lists_its_simulator() {
    let dir = scratch("sim-shown-late");
    let queue = turns(
        &dir,
        "shown",
        SHOW,
        &[
            showing(&[], &[]),
            showing(&[SHARED], &[]),
            showing(&[SHARED], &[UDID]),
            showing(&[SHARED, UDID], &[]),
        ],
    );
    let (config, xcrun) = listing_config(&dir, &queue, PROMPT, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let log = format!("{}.log", xcrun.display());
    let result = run(
        &rt,
        &cas,
        1,
        &asks(Spec::act(&format!("cat {log}")), DEVICE_TYPE),
    )
    .await;
    let result = result.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    assert_eq!(
        shows_in(&stdout(&cas, &result)),
        4,
        "xcodebuild looks before the action ran"
    );
    assert_eq!(
        shows_in(&std::fs::read_to_string(&log).expect("log")),
        4,
        "no look after the device was listed"
    );
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// What a node whose `xcodebuild` never lists the device gets, for `shown` (what the
/// fake prints for every look) and `listed` simulators in it: the lease fails as the
/// node's (`Failed`) once the wait is up, saying that `xcodebuild` did not list the
/// device, how many simulators it listed instead and its last listing; the action does
/// not run, and the device is deleted.
async fn xcodebuild_never_lists(name: &str, shown: String, listed: usize) {
    let dir = scratch(name);
    let file = dir.join("shown.txt");
    std::fs::write(&file, &shown).expect("shown");
    let extra = format!("{SHOW}\t-\tcat {}\n", file.display());
    let within = Duration::from_millis(300);
    let (config, xcrun) = listing_config(&dir, &extra, within, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let ran = dir.join("ran");
    let spec = asks(
        Spec::act(&format!("append {} x", ran.display())),
        DEVICE_TYPE,
    );
    let outcome = run(&rt, &cas, 1, &spec).await;
    let Err(RuntimeError::Failed(why)) = &outcome else {
        panic!("{outcome:?}");
    };
    let want = format!("simulator: Xcode did not list {UDID} within {within:?} (");
    assert!(why.starts_with(&want), "{why}");
    let seen =
        format!("looks); xcodebuild -showdestinations listed {listed} simulators, not {UDID}");
    assert!(why.contains(&seen), "{why}");
    assert!(
        why.contains("Available destinations"),
        "the last listing: {why}"
    );
    assert!(!ran.exists(), "the action did not run");
    assert!(
        shows_in(&std::fs::read_to_string(format!("{}.log", xcrun.display())).expect("log")) > 1,
        "looked again"
    );
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches a lease whose simulator `xcodebuild` never lists running its action anyway
/// (it would fail there, blamed on the action) while `xcdevice list` names the device
/// (the fake's default): the lease fails as the node's, the listing is the one
/// `xcodebuild` printed, with no simulator among the placeholders. Mutant planted: the
/// wait ends at the `xcdevice` look (red: the action ran).
#[tokio::test]
async fn a_node_whose_xcodebuild_lists_no_simulator_fails_the_lease_not_the_action() {
    xcodebuild_never_lists("sim-shown-none", showing(&[], &[]), 0).await;
}

/// Catches a listing of other simulators taken for one of the lease's (the 34-simulator
/// listing of run 38290129703, which lacked the new device): the device must be named,
/// not the list long. Mutant planted: `lists` matching any simulator id (red: the
/// action ran).
#[tokio::test]
async fn a_populated_listing_without_the_udid_is_not_listed() {
    xcodebuild_never_lists(
        "sim-shown-others",
        showing(&[SHARED, OTHER, NEIGHBOUR], &[UDID]),
        3,
    )
    .await;
}

/// Catches a look that stalls (one `xcodebuild -showdestinations` took about 80 s on a
/// hosted runner) holding the wait for as long as it likes: each look has
/// [`Listing::look`] to answer (200 ms here) and is killed after it, counted as not
/// listed, so the wait ends near [`Listing::within`] with the lease failed saying the
/// look did not answer. The fake's look sleeps 5 s. Mutant planted: the look given the
/// `simctl` answer time instead of [`Listing::look`] (red: the run takes the 5 s).
#[tokio::test]
async fn a_look_that_does_not_answer_is_cut_and_the_wait_still_ends() {
    let dir = scratch("sim-look-stalls");
    let extra = format!("{SHOW}\t-\tsleep 5\n");
    let (mut config, xcrun) = listing_config(
        &dir,
        &extra,
        Duration::from_millis(500),
        Duration::from_millis(10),
    );
    config.simulator_listing.look = Duration::from_millis(200);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let started = std::time::Instant::now();
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let Err(RuntimeError::Failed(why)) = &outcome else {
        panic!("{outcome:?}");
    };
    assert!(why.contains("no answer within 200ms"), "{why}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches the probe project left in the lease directory for the action to find, or
/// not written where the look runs: the look's `xcodebuild` runs in a directory that
/// holds the project, and the action's directory holds none of it. The fake
/// `xcodebuild` copies its working directory's listing to a file.
#[tokio::test]
async fn the_probe_project_is_there_for_the_looks_and_gone_for_the_action() {
    let dir = scratch("sim-probe");
    let seen = dir.join("seen");
    let extra = format!(
        "{SHOW}\t-\tcopy {} IOSFixture.xcodeproj/project.pbxproj ; cat {}/bin/showdestinations.txt\n",
        seen.display(),
        dir.display()
    );
    let (config, _) = listing_config(&dir, &extra, PROMPT, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let probe = lease_of(&config, 1).join(".kbf-simulator-probe");
    let spec = asks(
        Spec::act(&format!("test ! -e {}", probe.display())),
        DEVICE_TYPE,
    );
    let result = run(&rt, &cas, 1, &spec).await;
    let result = result.expect("ran");
    assert_eq!(result.exit_code, 0, "{}", stderr(&cas, &result));
    let project = std::fs::read_to_string(&seen).expect("the look ran where the project is");
    assert!(project.contains("IOSFixture"), "{project}");
    assert!(no_leases(&config));
}

/// Catches a kill during the wait for Xcode's listing not ending it (the kill would
/// wait up to the whole wait) or running the action after it: the run ends `Killed`,
/// the action not run, and the device is deleted.
#[tokio::test]
async fn a_kill_during_the_wait_for_the_listing_ends_it() {
    let dir = scratch("sim-kill-listing");
    let extra = format!("{LOOK}\t-\techo []\n");
    let (config, xcrun) = listing_config(&dir, &extra, PROMPT * 6, Duration::from_millis(10));
    let cas = Arc::new(MemoryCas::default());
    let rt = Arc::new(runtime(config.clone(), &cas));
    let ran = dir.join("ran");
    let action = asks(
        Spec::act(&format!("append {} x", ran.display())),
        DEVICE_TYPE,
    )
    .store(&cas);
    let running = tokio::spawn({
        let rt = Arc::clone(&rt);
        async move { rt.run(crate::support::work(1, action, 0)).await }
    });
    let waited = tokio::time::timeout(PROMPT, async {
        while looks(&xcrun) < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    waited.await.expect("the driver looks again");
    let killed = tokio::time::timeout(PROMPT, rt.kill(kbf_types::LeaseId::new(1, 1)));
    killed.await.expect("the kill ends the wait");
    let killed = running.await.expect("run task");
    assert!(matches!(killed, Err(RuntimeError::Killed)), "{killed:?}");
    assert!(!ran.exists(), "the action did not run");
    assert_eq!(calls(&xcrun), lease_calls(1, &[]));
    assert!(no_leases(&config));
}

/// Catches: a node where `xcrun` cannot be run (a Linux worker the action was placed
/// on without `OSFamily=darwin`, a Mac without developer tools) running the action
/// without the simulator it asked for, or failing it without saying why.
#[tokio::test]
async fn a_node_without_xcrun_fails_a_lease_that_asks_for_a_simulator() {
    let dir = scratch("sim-no-xcrun");
    let (mut config, _) = fake_config(&dir, "");
    config.xcrun = dir.join("no-xcrun");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl create kbf-lease-1-1 "),
        "{shown}"
    );
    assert!(no_leases(&config));
}

/// Catches: an action run without the simulator it asked for (a `simctl create` that
/// failed, or printed no UDID, taken for a made one), the failure blamed on the
/// action (it is the node's: a runtime it lacks) or not naming `simctl`'s reason; the
/// devices named for the lease not looked for after a create that failed (it may have
/// made one); and the lease directory left behind.
#[tokio::test]
async fn a_simulator_that_cannot_be_made_fails_the_lease_before_the_action_runs() {
    let dir = scratch("sim-create-fails");
    let (config, xcrun) = fake_config(&dir, "");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let ran = dir.join("ran");
    let action = Spec::act(&format!("append {} x", ran.display()));
    for (seq, device_type, why) in [
        (
            1,
            BAD_TYPE,
            "Invalid device type: com.apple.CoreSimulator.SimDeviceType.Nothing",
        ),
        (2, MUTE_TYPE, "simctl create printed no UDID"),
    ] {
        list(&xcrun, &[(&format!("kbf-lease-1-{seq}"), OTHER)]);
        let outcome = run(&rt, &cas, seq, &asks(action.clone(), device_type)).await;
        let shown = format!("{outcome:?}");
        assert!(
            shown.starts_with("Err(Failed(\"simulator: "),
            "{device_type}: {shown}"
        );
        assert!(shown.contains(why), "{device_type}: {shown}");
        assert_eq!(
            calls(&xcrun),
            [
                format!("create kbf-lease-1-{seq} {device_type} {RUNTIME}"),
                "list devices -j".to_owned(),
                format!("shutdown {OTHER}"),
                format!("delete {OTHER}"),
            ]
        );
        new_log(&xcrun);
    }
    assert!(!ran.exists(), "the action did not run");
    assert!(no_leases(&config), "the lease directories are removed");
}

/// Catches: a simulator that could not be deleted, or a device list that could not be
/// read, passed over in silence (the node fills with devices and nobody hears), or
/// the lease directory kept because of it.
#[tokio::test]
async fn a_simulator_that_cannot_be_deleted_fails_the_lease() {
    let dir = scratch("sim-delete-fails");
    let extra = "simctl list devices -j\tTMPDIR=*-1-2\techo not JSON\n";
    let (config, xcrun) = fake_config(&dir, extra);
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let outcome = run(&rt, &cas, 1, &asks(Spec::act("exit 0"), STUCK_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl delete ")
            && shown.contains("Unable to delete"),
        "{shown}"
    );
    assert_eq!(
        calls(&xcrun)[1..],
        [
            format!("bootstatus {STUCK} -b"),
            "list devices -j".to_owned(),
            format!("shutdown {STUCK}"),
            format!("delete {STUCK}")
        ]
    );
    let outcome = run(&rt, &cas, 2, &asks(Spec::act("exit 0"), DEVICE_TYPE)).await;
    let shown = format!("{outcome:?}");
    assert!(
        shown.starts_with("Err(Failed(\"simulator: simctl list devices -j: "),
        "{shown}"
    );
    assert_eq!(calls(&xcrun)[5..], lease_calls(2, &[]));
    assert!(
        no_leases(&config),
        "the lease directories are removed anyway"
    );
}

/// Catches: `simctl` run outside the actions' sandbox (the daemon would run a
/// developer tool, which reads `xcrun`'s cache that leases can write, unsandboxed) or
/// with another lease directory; `create` not given the Xcode the action names (its
/// runtimes are that Xcode's) or the boot not given it; and the end run with that Xcode or the daemon's own
/// `DEVELOPER_DIR` rather than the node's Xcode (so the start sweep, which knows no
/// Xcode, ends devices the same way).
#[tokio::test]
async fn simctl_runs_under_the_sandbox_with_the_actions_xcode() {
    let dir = scratch("sim-sandbox");
    let (mut config, xcrun) = fake_config(&dir, "");
    let sandbox_exec = dir.join("bin").join("sandbox-exec");
    kbf_testkit::act::link_self(&sandbox_exec).expect("link");
    std::fs::write(format!("{}.rules", sandbox_exec.display()), "").expect("rules");
    config.isolation = Isolation::Sandbox(sandbox_exec);
    let developer = "/Apps/Xcode_16.2.app/Contents/Developer";
    config.xcodes = [("16C5032a".to_owned(), PathBuf::from(developer))].into();
    let seen = dir.join("seen");
    let seen = seen.display();
    let devices = xcrun.with_file_name("devices.json");
    let extra = format!(
        "simctl create *\tDEVELOPER_DIR={developer}\tappend {seen} create ${{KBF_LEASE}} ; echo {UDID}\n\
         simctl create *\t-\techo --stderr not the actions Xcode ; exit 9\n\
         simctl bootstatus *\tDEVELOPER_DIR={developer}\tappend {seen} bootstatus ${{KBF_LEASE}}\n\
         simctl bootstatus *\t-\techo --stderr not the actions Xcode ; exit 9\n\
         simctl list *\tDEVELOPER_DIR=\tappend {seen} list ${{KBF_LEASE}} ; cat {}\n\
         simctl list *\t-\techo --stderr not the nodes Xcode ; exit 9\n\
         simctl shutdown *\tDEVELOPER_DIR=\tappend {seen} shutdown ${{KBF_LEASE}}\n\
         simctl delete *\tDEVELOPER_DIR=\tappend {seen} delete ${{KBF_LEASE}}\n\
         simctl delete *\t-\techo --stderr not the nodes Xcode ; exit 9\n",
        devices.display()
    );
    std::fs::write(format!("{}.rules", xcrun.display()), {
        let rules = std::fs::read_to_string(format!("{}.rules", xcrun.display())).expect("rules");
        format!("{extra}{rules}")
    })
    .expect("rules");
    let cas = Arc::new(MemoryCas::default());
    let rt = runtime(config.clone(), &cas);
    let spec = asks(Spec::act("exit 0"), DEVICE_TYPE).property("xcode", "16C5032a");
    let result = run(&rt, &cas, 1, &spec).await.expect("ran");
    assert_eq!(result.exit_code, 0);
    let lease = lease_of(&config, 1);
    let lease = lease.display();
    assert_eq!(
        std::fs::read_to_string(dir.join("seen")).expect("seen"),
        format!(
            "create {lease}\nbootstatus {lease}\nlist {lease}\nshutdown {lease}\ndelete {lease}\n"
        )
    );
}

/// Catches: a simulator a killed daemon's lease left never deleted (it would keep
/// running, maybe booted, and fill the node); a device of the shared set deleted (the
/// sweep must take only names that start `kbf-lease-`); the sweep run more than once,
/// or for a start whose left lease directories hold no marker (a start would ask
/// `simctl` every time); a marker followed through a link; and a device that cannot be
/// deleted, or a list that cannot be read, stopping the start (the daemon would stay
/// down over one simulator).
#[tokio::test]
async fn the_start_sweep_deletes_the_simulators_a_killed_daemon_left() {
    let dir = scratch("sim-sweep");
    let (config, xcrun) = fake_config(&dir, "");
    list(
        &xcrun,
        &[
            ("kbf-lease-7-7", UDID),
            ("iPhone 16", SHARED),
            ("kbf-lease-3-3", STUCK),
            ("kbf-lease-3-3 copy", OTHER),
        ],
    );
    let left = |name: &str, marker: bool| {
        let lease = config.scratch.join(name);
        std::fs::create_dir_all(lease.join("root")).expect("left");
        if marker {
            std::fs::write(lease.join(MARKER), b"").expect("marker");
        }
    };
    left("lease-7-7", true);
    left("lease-7-8", true);
    left("lease-7-9", false);
    let cas = Arc::new(MemoryCas::default());
    let _rt = runtime(config.clone(), &cas);
    assert_eq!(
        calls(&xcrun),
        [
            "list devices -j".to_owned(),
            format!("shutdown {UDID}"),
            format!("delete {UDID}"),
            format!("shutdown {STUCK}"),
            format!("delete {STUCK}"),
            format!("shutdown {OTHER}"),
            format!("delete {OTHER}"),
        ]
    );
    assert!(no_leases(&config), "every lease directory is removed");

    new_log(&xcrun);
    left("lease-7-9", false);
    let lease = config.scratch.join("lease-8-1");
    std::fs::create_dir_all(&lease).expect("left");
    std::os::unix::fs::symlink(dir.join("bin").join("devices.json"), lease.join(MARKER))
        .expect("link");
    let _rt = runtime(config.clone(), &cas);
    assert_eq!(calls(&xcrun), [""; 0], "no marker, no sweep");
    assert!(no_leases(&config));

    left("lease-8-2", true);
    list(&xcrun, &[("kbf-lease-8-2", UDID)]);
    let _rt = runtime(config.clone(), &cas);
    assert_eq!(
        calls(&xcrun),
        [
            "list devices -j".to_owned(),
            format!("shutdown {UDID}"),
            format!("delete {UDID}")
        ]
    );
    assert!(no_leases(&config));

    left("lease-9-9", true);
    let mut unlisted = config.clone();
    unlisted.xcrun = dir.join("no-xcrun");
    let _rt = runtime(unlisted, &cas);
    assert!(
        no_leases(&config),
        "a list that cannot be read does not stop the start"
    );
}

/// The tests that make real simulators.
#[cfg(target_os = "macos")]
mod real {
    use std::io::Write as _;
    use std::process::Command;

    use kbf_driver_native::NativeRuntime;

    use super::*;

    /// One real simulator at a time: two booting at once on a small runner is slow
    /// for no gain.
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// How long, in seconds, `xcodebuild test -destination` waits for its device to
    /// appear before it gives up: its `-destination-timeout`, 30 s by default. This is
    /// the contract the lease's device has to meet; a device listed only later fails a
    /// default `xcodebuild test`.
    const DESTINATION_TIMEOUT: u64 = 30;

    /// Whether [`a_real_simulator_is_made_booted_and_deleted_for_the_lease`] has run in
    /// this process: the sibling test says so in its log, to tell a listing that is
    /// late after a lease's device from one on a quiet node.
    static LEASE_TEST_RAN: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);

    /// How long one sandboxed `xcodebuild -showdestinations` try takes on a hosted
    /// runner: 15 s in the native-macos job (a try that listed the device at once
    /// reported itself at 15 s). A try sees a listing only when it ends, so a device
    /// that appears just before [`DESTINATION_TIMEOUT`] can be reported up to one try
    /// later.
    const ONE_TRY: u64 = 15;

    /// The latest elapsed time, in seconds since the first try started, at which the
    /// first try that lists the device may end and still meet the contract:
    /// [`DESTINATION_TIMEOUT`] plus one try's own duration. A try reports when it ends,
    /// so a passing value proves the device was listed no later than [`ONE_TRY`]
    /// seconds past the contract. The bound errs strict the other way: a device that
    /// appears while a try is already past its listing step waits for that try, the
    /// 5 s pause and a whole next try, so one that appears in the last seconds before
    /// [`DESTINATION_TIMEOUT`] can fail.
    const LISTED_BY: u64 = DESTINATION_TIMEOUT + ONE_TRY;

    /// How long, in seconds after its first try, the action keeps retrying
    /// `-showdestinations` while the device is not listed. Longer than [`LISTED_BY`]
    /// on purpose: a device that appears late still fails the test, but the per-try
    /// lines then say when it appeared (or that it never did, and whether any
    /// simulator was listed at all), which tells a late listing from a sandbox that
    /// lists nothing.
    const POLL_FOR: u64 = 120;

    /// `xcrun args`, unsandboxed (the test's own view), its stdout; panics on failure.
    fn xcrun(args: &[&str]) -> String {
        let out = Command::new("/usr/bin/xcrun")
            .args(args)
            .output()
            .expect("xcrun");
        assert!(
            out.status.success(),
            "xcrun {args:?}: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).expect("UTF-8")
    }

    /// The newest available iOS runtime and the first iPhone it supports, by
    /// identifier.
    fn pick() -> (String, String) {
        let list: serde_json::Value =
            serde_json::from_str(&xcrun(&["simctl", "list", "runtimes", "available", "-j"]))
                .expect("JSON");
        let runtime = list["runtimes"]
            .as_array()
            .expect("runtimes")
            .iter()
            .filter(|r| r["platform"] == "iOS")
            .max_by_key(|r| {
                let version = r["version"].as_str().unwrap_or("0");
                version
                    .split('.')
                    .map(|p| p.parse::<u32>().unwrap_or(0))
                    .collect::<Vec<_>>()
            })
            .expect("an iOS simulator runtime");
        let device_type = runtime["supportedDeviceTypes"]
            .as_array()
            .expect("device types")
            .iter()
            .find(|t| t["productFamily"] == "iPhone")
            .expect("an iPhone");
        let pair = (
            device_type["identifier"].as_str().expect("id").to_owned(),
            runtime["identifier"].as_str().expect("id").to_owned(),
        );
        let _ = writeln!(std::io::stderr(), "simulator: {} on {}", pair.0, pair.1);
        pair
    }

    /// Whether any process's arguments name `udid` (a booted simulator's
    /// `launchd_sim` does: it runs from the device's directory), looking for up to
    /// ten seconds for none.
    fn running(udid: &str) -> bool {
        for _ in 0..100 {
            if !running_now(udid) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        true
    }

    /// Whether a process's arguments name `udid` now.
    fn running_now(udid: &str) -> bool {
        Command::new("/usr/bin/pgrep")
            .args(["-f", udid])
            .output()
            .expect("pgrep")
            .status
            .success()
    }

    /// The iOS XCUITest fixtures (`tools/spike/macos/fixtures`), copied into `dir`; the
    /// `-project` path of the copy.
    fn ios_fixture(dir: &Path) -> String {
        let fixtures =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools/spike/macos/fixtures");
        let status = Command::new("/bin/cp")
            .arg("-R")
            .arg(&fixtures)
            .arg(dir)
            .status()
            .expect("cp");
        assert!(status.success(), "copy the fixtures");
        let project = dir.join("fixtures").join("IOSFixture.xcodeproj");
        assert!(project.is_dir(), "{}", project.display());
        project.to_str().expect("UTF-8").to_owned()
    }

    /// `xcodebuild args` on the iOS fixture at `project`, unsandboxed: whether it exited
    /// 0, and its stdout and stderr together.
    fn xcodebuild(project: &str, derived: &Path, args: &[&str]) -> (bool, String) {
        let out = Command::new("/usr/bin/xcodebuild")
            .args([
                "-project",
                project,
                "-scheme",
                "IOSFixture",
                "-derivedDataPath",
            ])
            .arg(derived)
            .args(args)
            .output()
            .expect("xcodebuild");
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.success(), text)
    }

    /// Settles, on a real Mac, where a lease's simulator must live: `xcodebuild
    /// -showdestinations` lists a device of the default set and not one made in a
    /// device set elsewhere (`simctl --set`), and a build with `-destination
    /// id=<UDID>` of the other set's device fails, unable to find it. Catches a design that makes the device where `xcodebuild
    /// test -destination` cannot reach it; the default-set arm is the control (a
    /// listing that never names any device would pass the other arm alone).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn xcodebuild_finds_a_device_of_the_default_set_and_not_one_of_another_set() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real-xcodebuild");
        let set = dir.join("set");
        std::fs::create_dir_all(&set).expect("set");
        let set = std::fs::canonicalize(&set).expect("real path");
        let set_arg = set.to_str().expect("UTF-8");
        let elsewhere = xcrun(&[
            "simctl",
            "--set",
            set_arg,
            "create",
            "kbf-probe-set",
            &device_type,
            &runtime_id,
        ]);
        let elsewhere = elsewhere.trim().to_owned();
        let default = xcrun(&[
            "simctl",
            "create",
            "kbf-probe-default",
            &device_type,
            &runtime_id,
        ]);
        let default = default.trim().to_owned();
        let project = ios_fixture(&dir);
        let derived = dir.join("derived");
        // Xcode can list no simulator for a while after a device is made (issue #608):
        // look again, as the lease's action does, and say what each look saw.
        let started = std::time::Instant::now();
        let mut try_ = 0;
        let shown = loop {
            try_ += 1;
            let (_, shown) = xcodebuild(&project, &derived, &["-showdestinations"]);
            let waited = started.elapsed().as_secs();
            let _ = writeln!(
                std::io::stderr(),
                "showdestinations try {try_} at {waited} s: lists the default set's device {}, \
                 iOS simulators listed {}; the lease test ran before: {}",
                shown.contains(&default),
                shown.matches("platform:iOS Simulator, arch").count(),
                LEASE_TEST_RAN.load(std::sync::atomic::Ordering::SeqCst)
            );
            if shown.contains(&default) || waited >= POLL_FOR {
                break shown;
            }
            std::thread::sleep(Duration::from_secs(5));
        };
        // A build that names the other set's device by id: `xcodebuild` resolves the
        // destination before it builds anything, so one it cannot find fails it at once.
        let destination = format!("id={elsewhere}");
        let (elsewhere_ok, elsewhere_build) =
            xcodebuild(&project, &derived, &["-destination", &destination, "build"]);
        xcrun(&["simctl", "delete", &default]);
        xcrun(&["simctl", "--set", set_arg, "delete", &elsewhere]);
        let _ = writeln!(
            std::io::stderr(),
            "xcodebuild -showdestinations:\n{shown}\n\
             build on the other set's device exited 0: {elsewhere_ok}:\n{}",
            (elsewhere_build.lines())
                .filter(|l| l.contains("rror") || l.contains("estination"))
                .take(8)
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(
            shown.contains(&default),
            "-showdestinations lists the default set's device"
        );
        assert!(
            !shown.contains(&elsewhere),
            "-showdestinations lists the other set's device"
        );
        assert!(!elsewhere_ok, "a destination in another set resolved");
        assert!(
            elsewhere_build.contains("Unable to find a "),
            "{elsewhere_build}"
        );
    }

    /// Catches, on a real Mac: `simctl create` in the default set not working under the
    /// actions' sandbox (create, or the delete at the end, refused), a device the
    /// action cannot boot from its sandbox, a device the action's own `xcodebuild
    /// -showdestinations` does not list from the sandbox, or lists only after
    /// [`LISTED_BY`] seconds (so `xcodebuild test -destination id=$KBF_SIMULATOR_UDID`
    /// with its default timeout could not use it), and a booted device left running or
    /// registered after the lease. The driver boots the device and waits before the action runs until
    /// `xcrun xcdevice list` names it and its own sandboxed `xcodebuild
    /// -showdestinations` lists it (issue #608); the action's `bootstatus -b` then
    /// returns at once, and a try that lists none prints how many devices are booted
    /// and the `CoreSimulator` processes. This checks that its wait stands
    /// for what the action's own `xcodebuild` sees: its first try is expected to list
    /// the device. The driver's own looks (their time and the simulators each listed)
    /// are in its log lines "xcodebuild -showdestinations look". The action keeps trying for [`POLL_FOR`] seconds;
    /// each try prints to the action's stderr what it saw: its exit, whether it named
    /// the lease's device, and how many iOS simulators it listed (none at all would
    /// point at the sandbox, not at a late listing); a run that never lists the device
    /// prints the last listing whole.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn a_real_simulator_is_made_booted_and_deleted_for_the_lease() {
        let _one = ONE.lock().await;
        LEASE_TEST_RAN.store(true, std::sync::atomic::Ordering::SeqCst);
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real");
        let fixtures = dir.join("fixtures");
        std::fs::create_dir_all(&fixtures).expect("fixtures");
        let project = ios_fixture(&fixtures);
        let config = config(&dir);
        let cas = Arc::new(MemoryCas::default());
        let rt = runtime(config.clone(), &cas);
        let script = format!(
            "set -e\n\
             echo \"${UDID_VAR}\"\n\
             xcrun simctl bootstatus \"${UDID_VAR}\" -b >&2\n\
             xcrun simctl list devices booted | grep -c \"${UDID_VAR}\"\n\
             cp -R {project} \"$TMPDIR/IOSFixture.xcodeproj\" >&2 || true\n\
             start=$(date +%s); try=0; found=0\n\
             while :; do\n\
               try=$((try + 1)); rc=0\n\
               xcodebuild -showdestinations -project \"$TMPDIR/IOSFixture.xcodeproj\" -scheme IOSFixture -derivedDataPath \"$TMPDIR/dd\" > \"$TMPDIR/shown\" 2>&1 || rc=$?\n\
               found=$(grep -c \"${UDID_VAR}\" \"$TMPDIR/shown\" || true)\n\
               sims=$(grep -c 'platform:iOS Simulator, arch' \"$TMPDIR/shown\" || true)\n\
               waited=$(($(date +%s) - start))\n\
               echo \"showdestinations try $try at $waited s: exit $rc, lists the lease's device $found, iOS simulators listed $sims\" >&2\n\
               if [ \"$found\" != 1 ]; then\n\
                 xcrun simctl list devices 2>&1 | grep -c Booted >&2 || true\n\
                 pgrep -fl CoreSimulator >&2 || true\n\
               fi\n\
               if [ \"$found\" = 1 ] || [ \"$waited\" -ge {POLL_FOR} ]; then break; fi\n\
               sleep 5\n\
             done\n\
             [ \"$found\" = 1 ] || cat \"$TMPDIR/shown\" >&2\n\
             echo \"$found\"\n\
             if [ \"$found\" = 1 ]; then echo \"$waited\"; else echo never; fi\n\
             grep -i -m 8 -e error -e denied \"$TMPDIR/shown\" >&2 || true\n"
        );
        let spec = Spec::sh(&script)
            .property("simulator", &format!("{device_type},{runtime_id}"))
            .timeout(Duration::from_secs(600));
        let action = spec.store(&cas);
        let result = tokio::time::timeout(
            Duration::from_secs(660),
            rt.run(crate::support::work(1, action, 0)),
        )
        .await
        .expect("the run ends")
        .expect("ran");
        let err = stderr(&cas, &result);
        let _ = writeln!(std::io::stderr(), "action stderr:\n{err}");
        assert_eq!(result.exit_code, 0, "{err}");
        let out = stdout(&cas, &result);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "{out}");
        let udid = lines[0];
        assert_eq!(lines[1], "1", "the device booted");
        assert_eq!(
            lines[2], "1",
            "xcodebuild lists the device from the sandbox within {POLL_FOR} s"
        );
        let listed_at: u64 = lines[3].parse().expect("seconds to the first listing");
        assert!(
            listed_at <= LISTED_BY,
            "xcodebuild first listed the device at {listed_at} s, after the {LISTED_BY} s \
             a default `xcodebuild test -destination` allows ({DESTINATION_TIMEOUT} s \
             -destination-timeout plus one {ONE_TRY} s try)"
        );
        assert!(
            !xcrun(&["simctl", "list", "devices", "-j"]).contains(udid),
            "the device is deleted"
        );
        assert!(!running(udid), "the device is shut down");
        assert!(no_leases(&config), "the lease directory is removed");
    }

    /// Catches, on a real Mac: a booted simulator a killed daemon's lease left not shut
    /// down and deleted by the next start (it would keep running and fill the node).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "macOS with an iOS simulator runtime; the native-macos job runs it"]
    async fn the_start_deletes_a_real_booted_simulator_left_behind() {
        let _one = ONE.lock().await;
        let (device_type, runtime_id) = pick();
        let dir = scratch("sim-real-left");
        let config = config(&dir);
        let lease = config.scratch.join("lease-9-9");
        std::fs::create_dir_all(&lease).expect("lease");
        std::fs::write(lease.join(MARKER), b"").expect("marker");
        let created = xcrun(&[
            "simctl",
            "create",
            "kbf-lease-9-9",
            &device_type,
            &runtime_id,
        ]);
        let udid = created.trim().to_owned();
        // `boot` returns once the device is booted, without the minutes of data
        // migration `bootstatus` waits for: enough for a device left running.
        xcrun(&["simctl", "boot", &udid]);
        assert!(
            running_now(&udid),
            "pgrep finds a booted simulator by its UDID"
        );
        let cas = Arc::new(MemoryCas::default());
        let _rt = tokio::task::spawn_blocking({
            let config = config.clone();
            move || NativeRuntime::new(config, cas).expect("runtime")
        })
        .await
        .expect("start");
        assert!(!running(&udid), "the left device is shut down");
        assert!(
            !xcrun(&["simctl", "list", "devices", "-j"]).contains(&udid),
            "the left device is deleted"
        );
        assert!(no_leases(&config), "the left lease directory is removed");
    }
}
