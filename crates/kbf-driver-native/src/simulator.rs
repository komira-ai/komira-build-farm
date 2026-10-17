//! A simulator of the lease's own: an action that names the platform property
//! [`PROPERTY`] (`simulator=<device type>,<runtime>`) gets a new simulator, made for it
//! when its lease starts and deleted when the lease ends. The device is in the daemon
//! user's default device set (`~/Library/Developer/CoreSimulator/Devices`), named
//! [`NAME_PREFIX`]`<term>-<seq>` for its lease: `xcodebuild -destination` finds only the
//! devices of that set, not those of a set elsewhere (`simctl --set`).
//!
//! - **At start**, after the inputs are written and before the action runs, the driver
//!   writes [`MARKER`] in the lease directory, then runs `xcrun simctl create
//!   kbf-lease-<term>-<seq> <device type> <runtime>` (with the action's
//!   `DEVELOPER_DIR` when it names an `xcode`). It must exit 0 within [`ANSWER_WITHIN`]
//!   and print the new device's UDID; otherwise the lease fails
//!   ([`RuntimeError::Failed`]: a runtime the node lacks is the node's to fix, and the
//!   action is not run). The action finds the UDID in [`UDID_VAR`], which wins over the
//!   Command's environment. The device is made shut down: the action boots it (`xcrun
//!   simctl boot "$KBF_SIMULATOR_UDID"`) or lets `xcodebuild` do it.
//! - **Then, before the action runs**, the driver waits until Xcode lists the device,
//!   as the action asks it: each look runs, as the action would run them (its sandbox,
//!   its Xcode), first `xcrun xcdevice list` (cheap) and, once that names the UDID,
//!   `xcrun xcodebuild -showdestinations` on a probe package the driver writes in the
//!   lease directory (removed before the action runs), which must name the UDID among
//!   its available destinations. Looks follow every [`Listing::every`] for up to
//!   [`Listing::within`] ([`Listing::DEFAULT`]: 15 s and 300 s), each look having
//!   [`Listing::look`] to answer. For a while on some Macs (on hosted runners, from a
//!   few seconds to over two minutes), Xcode lists no simulator at all, though `simctl`
//!   and `xcdevice list` already show the device: only the second look is the
//!   `xcodebuild` the action runs, and an `xcodebuild test -destination
//!   id=$KBF_SIMULATOR_UDID` started then gives up after its own 30 s
//!   (`-destination-timeout`) and fails the action. A device not listed in time fails
//!   the lease ([`RuntimeError::Failed`], the node's to fix) with the last listing,
//!   and the action is not run; a kill ends the wait between two looks.
//! - **At the end**, whatever the outcome (an exit, a failure, a timeout, a kill, a run
//!   the daemon drops), before the lease directory is removed: the device the create
//!   printed, and every device named for the lease in `simctl list devices` (one a
//!   create cut short made late), is shut down and then deleted, by UDID, with the
//!   node's own Xcode. The UDID the create printed is kept by the daemon, so an action
//!   that renames its device or edits its lease directory cannot point the delete at
//!   another device. A shutdown that fails is not looked at (a device that is not
//!   booted refuses one); a delete that fails fails the lease, as a lease directory
//!   that stays does, so a dirty node is loud. Each command names one device, never
//!   `all`.
//! - **At daemon start**, when a lease directory a killed daemon left holds [`MARKER`]
//!   (a regular file, looked at without following a link), the start writes [`PENDING`]
//!   in the scratch root before it removes that directory, and returns: it never waits
//!   for `simctl`, which asks `CoreSimulatorService`, and that can stall for minutes.
//!   While [`PENDING`] is there, a thread of its own ([`Background`]) shuts down and
//!   deletes every device in the default set whose name starts with [`NAME_PREFIX`],
//!   except those of leases that run now, with `simctl` run in a directory
//!   [`SWEEP_DIR`] of the scratch root. A clean sweep removes [`PENDING`]. One that
//!   fails (the listing did not answer, a delete failed) keeps it and tries again after
//!   [`RETRY_AFTER`], up to [`SWEEP_ATTEMPTS`] times; if all fail, the next start
//!   sweeps again. A daemon that was never killed during a simulator lease and owes
//!   nothing asks `simctl` nothing at start.
//!
//! `simctl` runs as an action does ([`crate::xcode::Sandbox`]): under the actions'
//! sandbox with the network off and the lease directory as its own, so the daemon runs
//! no developer tool outside the sandbox. `simctl` asks `CoreSimulatorService`, which
//! runs outside the sandbox, to make, boot and delete the device. So do the action's
//! own `simctl` and `xcodebuild`: the processes of a booted simulator run under that
//! service, outside the action's sandbox and its network-off rule, and the action can
//! reach every device of the default set, not only its own.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

use crate::network::Isolation;
use crate::xcode::{self, DEVELOPER_DIR, Sandbox};
use tokio::sync::oneshot;

/// The platform property that asks for a simulator of the lease's own.
pub const PROPERTY: &str = "simulator";

/// The variable that names the lease's simulator's UDID to the action.
pub const UDID_VAR: &str = "KBF_SIMULATOR_UDID";

/// How the name of every lease's simulator starts; the rest is `<term>-<seq>`.
pub const NAME_PREFIX: &str = "kbf-lease-";

/// The file in the lease directory that says the lease may have made a simulator:
/// written before the create, so the start after a killed daemon sweeps the devices.
pub const MARKER: &str = "simulator";

/// How long each `simctl` command has to exit.
pub const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// How many times the background sweep runs its listing and deletes before it leaves
/// the devices a killed daemon left to the next start. A `CoreSimulatorService` that
/// stalls answers no `simctl` for minutes and then answers again, so a try is made
/// again only after [`RETRY_AFTER`], not at once.
pub const SWEEP_ATTEMPTS: u32 = 5;

/// How long the background sweep waits after a try that failed before the next one
/// ([`NativeConfig::simulator_retry`](crate::NativeConfig::simulator_retry)).
pub const RETRY_AFTER: Duration = Duration::from_secs(120);

/// The file in the scratch root that says devices a killed daemon left may still be
/// there: written at start before the lease directory that held the [`MARKER`] is
/// removed (the only other record of them), and removed by the first sweep that ends
/// clean. A start that finds it sweeps again.
pub const PENDING: &str = "simulators-left";

/// The directory in the scratch root the background sweep runs `simctl` in (as its
/// lease directory), made when a sweep is owed and kept.
pub const SWEEP_DIR: &str = "simulator-sweep";

/// How long the driver waits for Xcode to list a lease's new simulator, how often it
/// looks and how long each look has, before the action runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Listing {
    /// How long after the device is made it must be listed, or the lease fails.
    pub within: Duration,
    /// The pause between two looks.
    pub every: Duration,
    /// How long one look has to answer, before it is killed and counts as not listed.
    pub look: Duration,
}

impl Listing {
    /// 300 s, looking every 15 s, each look given [`ANSWER_WITHIN`]. On hosted macOS
    /// runners (issue #608), the action's own sandboxed `xcodebuild -showdestinations`
    /// first listed its simulator from a few seconds to 155 s after it began looking,
    /// and sometimes not within 126 s, while it listed no simulator at all; one call
    /// took up to 80 s. About twice the longest first listing keeps a slow node serving;
    /// a node that cannot list a device in five minutes fails the lease rather than the
    /// action. A look of `xcodebuild` takes about 15 s there, so a pause of that size
    /// keeps the node from running them back to back.
    pub const DEFAULT: Self = Self {
        within: Duration::from_secs(300),
        every: Duration::from_secs(15),
        look: ANSWER_WITHIN,
    };
}

/// How [`Simctl::wait_listed`] ended.
#[derive(Debug)]
pub(crate) enum Waited<T> {
    /// The device was listed, this long after the wait began, at this look.
    Listed(Duration, u32),
    /// It was not listed within [`Listing::within`]: why the last look did not.
    Never(String),
    /// A kill arrived; what it sent.
    Stopped(T),
}

/// The simulator an action asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// A device type, by identifier
    /// (`com.apple.CoreSimulator.SimDeviceType.iPhone-16`) or name (`iPhone 16`).
    pub device_type: String,
    /// A runtime, by identifier (`com.apple.CoreSimulator.SimRuntime.iOS-18-4`).
    pub runtime: String,
}

/// The simulator the action's platform (the Action's, or the Command's for clients
/// older than REAPI 2.2) asks for with [`PROPERTY`], its name read in any case; `None`
/// when it asks for none.
///
/// # Errors
/// [`RuntimeError::Invalid`] for a value that is not `<device type>,<runtime>`, both
/// parts not empty: the client's to fix.
#[allow(deprecated)]
pub fn request_of(action: &Action, command: &Command) -> Result<Option<Request>, RuntimeError> {
    let platform: Option<&Platform> = action
        .platform
        .as_ref()
        .filter(|p| !p.properties.is_empty())
        .or(command.platform.as_ref());
    let Some(value) = platform
        .into_iter()
        .flat_map(|p| &p.properties)
        .find(|p| p.name.eq_ignore_ascii_case(PROPERTY))
        .map(|p| p.value.as_str())
    else {
        return Ok(None);
    };
    match value.split_once(',') {
        Some((device_type, runtime))
            if !device_type.is_empty() && !runtime.is_empty() && !runtime.contains(',') =>
        {
            Ok(Some(Request {
                device_type: device_type.to_owned(),
                runtime: runtime.to_owned(),
            }))
        }
        _ => Err(RuntimeError::Invalid(format!(
            "platform property {PROPERTY}={value:?}: use <device type>,<runtime>"
        ))),
    }
}

/// How the driver runs `simctl`: this `xcrun`, under this isolation with these
/// user-folder rules.
#[derive(Debug)]
pub(crate) struct Simctl {
    pub xcrun: PathBuf,
    pub isolation: Isolation,
    pub rules: String,
    pub within: Duration,
    pub listing: Listing,
    /// The names of the devices the leases that run now were given, from before their
    /// create to after their delete: the background sweep leaves them alone.
    pub live: Mutex<BTreeSet<String>>,
}

/// The simulator a lease may have: what its end needs, set before the create runs.
#[derive(Clone, Debug)]
pub(crate) struct Made {
    /// The lease directory's real path, the sandbox's directory for `simctl`.
    pub lease: PathBuf,
    /// The device's name, [`NAME_PREFIX`]`<term>-<seq>`.
    pub name: String,
    /// The UDID the create printed, once it did.
    pub udid: Option<String>,
}

impl Simctl {
    /// `xcrun simctl args` under the sandbox of `lease` (a real path), with the Xcode
    /// at `developer_dir`, or the node's own with the daemon's `DEVELOPER_DIR` removed.
    fn run(
        &self,
        lease: &Path,
        developer_dir: Option<&Path>,
        args: &[&str],
    ) -> Result<std::process::Output, String> {
        let mut all = vec!["simctl"];
        all.extend_from_slice(args);
        self.xcrun(lease, developer_dir, &all)
    }

    /// `xcrun all` under the sandbox of `lease` (a real path), as [`Self::run`] runs
    /// `simctl`.
    fn xcrun(
        &self,
        lease: &Path,
        developer_dir: Option<&Path>,
        all: &[&str],
    ) -> Result<std::process::Output, String> {
        self.xcrun_in(lease, developer_dir, all, self.within, None)
    }

    /// [`Self::xcrun`] with `within` to answer, run in the directory `cwd` when one is
    /// named.
    fn xcrun_in(
        &self,
        lease: &Path,
        developer_dir: Option<&Path>,
        all: &[&str],
        within: Duration,
        cwd: Option<&Path>,
    ) -> Result<std::process::Output, String> {
        let sandbox = Sandbox {
            isolation: self.isolation.clone(),
            dir: lease.to_owned(),
            rules: self.rules.clone(),
        };
        let mut command = sandbox.command(&self.xcrun, all);
        match developer_dir {
            Some(dir) => command.env(DEVELOPER_DIR, dir),
            None => command.env_remove(DEVELOPER_DIR),
        };
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        let shown = || all.join(" ");
        let out = xcode::output_within(command, within, lease)
            .map_err(|e| format!("{}: {e}", shown()))?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(format!(
                "{}: {}: {}",
                shown(),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }

    /// Makes a new simulator named `made.name`; its UDID. The caller has written
    /// [`MARKER`] in `made.lease`.
    pub(crate) fn create(
        &self,
        made: &Made,
        request: &Request,
        developer_dir: Option<&Path>,
    ) -> Result<String, String> {
        let args = ["create", &made.name, &request.device_type, &request.runtime];
        let out = self.run(&made.lease, developer_dir, &args)?;
        let printed = String::from_utf8_lossy(&out.stdout);
        let udid = printed.trim();
        if is_udid(udid) {
            Ok(udid.to_owned())
        } else {
            Err(format!("simctl create printed no UDID: {udid:?}"))
        }
    }

    /// One look: `Ok` when `xcrun xcdevice list` (Xcode's own list of the devices and
    /// simulators it can use) names `udid` and then `xcrun xcodebuild -showdestinations`
    /// on the project in `probe` (what [`write_probe`] wrote) lists it as an available destination,
    /// each run with the Xcode at `developer_dir`; otherwise what the first that did
    /// not printed, or why it failed. The second is the question the action asks: the
    /// first can answer while it lists no simulator at all.
    fn listed(
        &self,
        lease: &Path,
        probe: &Path,
        udid: &str,
        developer_dir: Option<&Path>,
    ) -> Result<(), String> {
        let started = Instant::now();
        let args = ["xcdevice", "list", "--timeout", XCDEVICE_TIMEOUT];
        let out = self.xcrun_in(lease, developer_dir, &args, self.listing.look, None)?;
        if !lists(&out.stdout, udid) {
            let printed = String::from_utf8_lossy(&out.stdout);
            return Err(format!(
                "the last listing: {}",
                xcode::cut(printed.trim(), LISTING_BYTES)
            ));
        }
        let derived = probe.join("dd");
        let project = probe.join(format!("{PROBE_SCHEME}.xcodeproj"));
        let args = [
            "xcodebuild",
            "-showdestinations",
            "-project",
            &project.to_string_lossy(),
            "-scheme",
            PROBE_SCHEME,
            "-derivedDataPath",
            &derived.to_string_lossy(),
        ];
        let out = self.xcrun_in(lease, developer_dir, &args, self.listing.look, Some(probe))?;
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let (listed, simulators) = destinations(&printed, udid);
        tracing::info!(
            %udid,
            ms = started.elapsed().as_millis(),
            simulators,
            listed,
            "xcodebuild -showdestinations look"
        );
        if listed {
            Ok(())
        } else {
            Err(format!(
                "xcodebuild -showdestinations listed {simulators} simulators, not {udid}: {}",
                xcode::cut(printed.trim(), LISTING_BYTES)
            ))
        }
    }

    /// Waits until Xcode lists `udid`, made in `lease` (a real path) with the Xcode at
    /// `developer_dir`, looking as [`Self::listed`] does, at once and then every
    /// [`Listing::every`] for up to [`Listing::within`]. Each look has [`Listing::look`]
    /// to answer, the last one too (one cut short by the deadline would say nothing of
    /// the listing), so the wait can last that much longer. The probe project the looks
    /// run on is written in `lease` before the first and removed after the last. A kill
    /// (`stop`) is taken between two looks, never during one: a look runs in the lease directory,
    /// which the end removes.
    pub(crate) async fn wait_listed<T>(
        self: &Arc<Self>,
        lease: &Path,
        udid: &str,
        developer_dir: Option<&Path>,
        stop: &mut oneshot::Receiver<T>,
    ) -> Waited<T> {
        let probe = lease.join(PROBE_DIR);
        write_probe(&probe);
        let waited = self
            .wait_looks(lease, &probe, udid, developer_dir, stop)
            .await;
        let _ = std::fs::remove_dir_all(&probe);
        waited
    }

    /// [`Self::wait_listed`]'s loop of looks, on the probe project at `probe`.
    async fn wait_looks<T>(
        self: &Arc<Self>,
        lease: &Path,
        probe: &Path,
        udid: &str,
        developer_dir: Option<&Path>,
        stop: &mut oneshot::Receiver<T>,
    ) -> Waited<T> {
        let started = Instant::now();
        let listing = self.listing;
        let mut looks = 0;
        loop {
            looks += 1;
            let look = tokio::task::spawn_blocking({
                let simctl = Arc::clone(self);
                let (lease, probe, udid) = (lease.to_owned(), probe.to_owned(), udid.to_owned());
                let developer_dir = developer_dir.map(Path::to_owned);
                move || simctl.listed(&lease, &probe, &udid, developer_dir.as_deref())
            });
            // No closure for the panic: a look that never panics would leave it unrun.
            let look = look.await.unwrap_or(Err(String::from("the look panicked")));
            let Err(why) = look else {
                return Waited::Listed(started.elapsed(), looks);
            };
            let left = listing.within.saturating_sub(started.elapsed());
            if left.is_zero() {
                return Waited::Never(format!(
                    "Xcode did not list {udid} within {:?} ({looks} looks); {why}",
                    listing.within
                ));
            }
            tokio::select! {
                () = tokio::time::sleep(listing.every.min(left)) => {}
                Ok(by) = &mut *stop => return Waited::Stopped(by),
            }
        }
    }

    /// Shuts down and deletes the lease's simulator: the device the create printed and
    /// every device named for the lease; why each delete that failed, or the listing,
    /// did. The end of every lease that asked for a simulator.
    pub(crate) fn end(&self, made: &Made) -> Result<(), String> {
        let ended = self.delete_all(made);
        self.live().remove(&made.name);
        ended
    }

    /// The names of the devices of the leases that run now, locked.
    fn live(&self) -> std::sync::MutexGuard<'_, BTreeSet<String>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records that the lease named `name` may make a device, before its create runs:
    /// the background sweep skips it until [`Self::end`].
    pub(crate) fn hold(&self, name: &str) {
        self.live().insert(name.to_owned());
    }

    fn delete_all(&self, made: &Made) -> Result<(), String> {
        let named = self.devices(&made.lease);
        let mut udids: Vec<String> = made.udid.iter().cloned().collect();
        let mut failed = Vec::new();
        match named {
            Ok(named) => {
                for (name, udid) in named {
                    if name == made.name && !udids.contains(&udid) {
                        udids.push(udid);
                    }
                }
            }
            Err(why) => failed.push(why),
        }
        failed.extend(self.delete(&made.lease, &udids));
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// Shuts down and deletes every device whose name starts with [`NAME_PREFIX`],
    /// running `simctl` in the lease directory `lease` (a real path) a killed daemon
    /// left; why the
    /// listing or each delete that failed did. The start after a killed daemon left a
    /// lease that may have made one.
    pub(crate) fn sweep(&self, lease: &Path) -> Result<(), String> {
        let listed = self.devices(lease)?;
        // After the listing: a lease that made its device before the listing holds its
        // name by now.
        let live = self.live();
        let udids: Vec<String> = (listed.into_iter())
            .filter(|(name, _)| name.starts_with(NAME_PREFIX) && !live.contains(name))
            .map(|(_, udid)| udid)
            .collect();
        drop(live);
        let failed = self.delete(lease, &udids);
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    /// One try of the sweep [`PENDING`] asks for, in [`SWEEP_DIR`] of `scratch`; a
    /// clean end removes [`PENDING`] (one that stays only makes the next start sweep
    /// again).
    fn sweep_owed(&self, scratch: &Path) -> Result<(), String> {
        let dir = scratch.join(SWEEP_DIR);
        // The sandbox's directory is a real path, every link resolved.
        let real = std::fs::create_dir_all(&dir).and_then(|()| std::fs::canonicalize(&dir));
        let Ok(real) = real else {
            return Err(format!(
                "no directory to sweep simulators in: {}",
                dir.display()
            ));
        };
        self.sweep(&real)?;
        let _ = std::fs::remove_file(scratch.join(PENDING));
        Ok(())
    }

    /// Shuts down and deletes each of `udids`, one command each; why each delete that
    /// failed did.
    fn delete(&self, dir: &Path, udids: &[String]) -> Vec<String> {
        let mut failed = Vec::new();
        for udid in udids {
            // A device that is not booted refuses a shutdown: only the delete counts.
            let _ = self.run(dir, None, &["shutdown", udid]);
            if let Err(why) = self.run(dir, None, &["delete", udid]) {
                failed.push(why);
            }
        }
        failed
    }

    /// The name and UDID of every device `simctl list devices -j` prints whose UDID
    /// is one, in its order.
    fn devices(&self, dir: &Path) -> Result<Vec<(String, String)>, String> {
        let out = self.run(dir, None, &["list", "devices", "-j"])?;
        devices_in(&out.stdout).map_err(|e| format!("simctl list devices -j: {e}"))
    }
}

/// How long `xcdevice list` waits for devices to show up (`--timeout`, seconds).
const XCDEVICE_TIMEOUT: &str = "5";

/// The directory in the lease directory that holds the probe project, removed after
/// the wait for the listing.
const PROBE_DIR: &str = ".kbf-simulator-probe";

/// The probe project's name, and its scheme's: an iOS app project (a copy of the
/// `IOSFixture` of `tools/spike/macos/fixtures`, which the native-macos job runs
/// `xcodebuild` on under the actions' sandbox). A Swift package would need a package
/// cache the sandbox does not let `xcodebuild` write.
const PROBE_SCHEME: &str = "IOSFixture";

/// The files of the probe project, by path in `<scheme>.xcodeproj`.
const PROBE_FILES: [(&str, &str); 2] = [
    (
        "project.pbxproj",
        include_str!("../probe/IOSFixture.xcodeproj/project.pbxproj"),
    ),
    (
        "xcshareddata/xcschemes/IOSFixture.xcscheme",
        include_str!("../probe/IOSFixture.xcodeproj/xcshareddata/xcschemes/IOSFixture.xcscheme"),
    ),
];

/// Writes the probe project into `probe`. A file that cannot be written fails every
/// look, and the last listing says why (`xcodebuild` finds no project), so the
/// errors need no branch of their own.
fn write_probe(probe: &Path) {
    let project = probe.join(format!("{PROBE_SCHEME}.xcodeproj"));
    let _ = std::fs::create_dir_all(project.join("xcshareddata/xcschemes"));
    for (path, text) in PROBE_FILES {
        let _ = std::fs::write(project.join(path), text);
    }
}

/// How much of the last listing a lease that never saw its device says.
const LISTING_BYTES: usize = 1000;

/// Whether `json`, what `xcdevice list` printed (an array of devices), holds a device
/// whose `identifier` is `udid` and that is not marked `"available": false`.
fn lists(json: &[u8], udid: &str) -> bool {
    let Ok(serde_json::Value::Array(devices)) = serde_json::from_slice(json) else {
        return false;
    };
    devices.iter().any(|device| {
        device.get("identifier").and_then(serde_json::Value::as_str) == Some(udid)
            && device.get("available").and_then(serde_json::Value::as_bool) != Some(false)
    })
}

/// What `xcodebuild -showdestinations` printed, `text`: whether it lists `udid` as an
/// available destination (a destination line `{ platform:.., id:<udid>, .. }` before the
/// "Ineligible destinations" heading, the id whole and in any case), and how many
/// simulators (destinations whose id is a UDID) it lists so; the placeholders it
/// always prints do not count.
fn destinations(text: &str, udid: &str) -> (bool, usize) {
    let eligible = text
        .split("Ineligible destinations")
        .next()
        .unwrap_or_default();
    let mut simulators = 0;
    let mut listed = false;
    for line in eligible.lines().filter(|l| l.trim_start().starts_with('{')) {
        let id = line
            .split([',', '}'])
            .filter_map(|part| {
                part.trim()
                    .trim_start_matches('{')
                    .trim_start()
                    .strip_prefix("id:")
            })
            .next();
        if let Some(id) = id.filter(|id| is_udid(id)) {
            simulators += 1;
            listed |= id.eq_ignore_ascii_case(udid);
        }
    }
    (listed, simulators)
}

/// [`Simctl::devices`] from what `simctl list devices -j` printed:
/// `{"devices": {"<runtime>": [{"name": .., "udid": ..}, ..], ..}}`.
fn devices_in(json: &[u8]) -> Result<Vec<(String, String)>, String> {
    let list: serde_json::Value = serde_json::from_slice(json).map_err(|e| e.to_string())?;
    let Some(runtimes) = list.get("devices").and_then(serde_json::Value::as_object) else {
        return Err("no \"devices\" object".to_owned());
    };
    let mut found = Vec::new();
    for device in runtimes
        .values()
        .filter_map(serde_json::Value::as_array)
        .flatten()
    {
        let field = |key: &str| device.get(key).and_then(serde_json::Value::as_str);
        if let (Some(name), Some(udid)) = (field("name"), field("udid"))
            && is_udid(udid)
        {
            found.push((name.to_owned(), udid.to_owned()));
        }
    }
    Ok(found)
}

/// Whether the lease directory at `lease` holds [`MARKER`] as a regular file, looked at
/// without following a link.
pub(crate) fn marked(lease: &Path) -> bool {
    std::fs::symlink_metadata(lease.join(MARKER)).is_ok_and(|m| m.file_type().is_file())
}

/// Whether an earlier start left [`PENDING`] in `scratch` as a regular file, looked at
/// without following a link.
pub(crate) fn pending(scratch: &Path) -> bool {
    std::fs::symlink_metadata(scratch.join(PENDING)).is_ok_and(|m| m.file_type().is_file())
}

/// Writes [`PENDING`] in `scratch`: devices a killed daemon left may still be there.
pub(crate) fn owe(scratch: &Path) -> std::io::Result<()> {
    std::fs::write(scratch.join(PENDING), b"")
}

/// The start's sweep of the devices a killed daemon left, on a thread of its own so
/// that a stalled `CoreSimulatorService` cannot hold the start.
#[derive(Debug, Default)]
pub(crate) struct Background {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

#[derive(Debug, Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct State {
    /// The thread is still at its tries.
    running: bool,
    /// The runtime is gone: no more tries.
    stop: bool,
}

impl Shared {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Waits `retry` for the runtime to go; whether it did not.
    fn pause(&self, retry: Duration) -> bool {
        let state = self.state();
        let (state, _) = (self.changed)
            .wait_timeout_while(state, retry, |s| !s.stop)
            .unwrap_or_else(PoisonError::into_inner);
        !state.stop
    }

    fn finish(&self) {
        self.state().running = false;
        self.changed.notify_all();
    }
}

impl Background {
    /// Starts the sweep [`PENDING`] in `scratch` asks for: up to [`SWEEP_ATTEMPTS`]
    /// tries, `retry` apart, until one ends clean. Like [`std::thread::spawn`], it
    /// panics if the OS cannot make a thread.
    pub(crate) fn start(&self, simctl: &Arc<Simctl>, scratch: &Path, retry: Duration) {
        self.shared.state().running = true;
        let (shared, simctl, scratch) = (
            Arc::clone(&self.shared),
            Arc::clone(simctl),
            scratch.to_owned(),
        );
        let thread = std::thread::spawn(move || {
            for attempt in 1..=SWEEP_ATTEMPTS {
                if attempt > 1 && !shared.pause(retry) {
                    break;
                }
                match simctl.sweep_owed(&scratch) {
                    Ok(()) => break,
                    Err(why) if attempt < SWEEP_ATTEMPTS => {
                        tracing::warn!(attempt, "simulators left behind: {why}");
                    }
                    Err(why) => tracing::error!(
                        "simulators left behind, tried again at the next start: {why}"
                    ),
                }
            }
            shared.finish();
        });
        *self.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(thread);
    }

    /// Waits until the sweep has ended (clean, out of tries, or never started), at
    /// most `within`; whether it has.
    pub(crate) fn wait(&self, within: Duration) -> bool {
        let state = self.shared.state();
        let (state, _) = (self.shared.changed)
            .wait_timeout_while(state, within, |s| s.running)
            .unwrap_or_else(PoisonError::into_inner);
        !state.running
    }
}

/// Ends the sweep's tries and waits for its thread: at most the `simctl` call it is
/// in ([`ANSWER_WITHIN`]), as it never waits for the next try once stopped.
impl Drop for Background {
    fn drop(&mut self) {
        self.shared.state().stop = true;
        self.shared.changed.notify_all();
        let thread = self
            .thread
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
    }
}

/// Whether `text` is a UDID as `simctl` prints one: 8-4-4-4-12 hexadecimal digits.
fn is_udid(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    fn with(name: &str, value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: name.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the property read only in one spelling or only from the Action, a
    /// value split anywhere but at its one comma, and a malformed value let through to
    /// `simctl` (which would fail the lease as the node's fault, not the client's).
    #[test]
    #[allow(deprecated)]
    fn the_request_is_a_device_type_and_a_runtime() {
        let want = Request {
            device_type: "iPhone 16".to_owned(),
            runtime: "com.apple.CoreSimulator.SimRuntime.iOS-18-4".to_owned(),
        };
        let value = "iPhone 16,com.apple.CoreSimulator.SimRuntime.iOS-18-4";
        for name in ["simulator", "Simulator", "SIMULATOR"] {
            let action = Action {
                platform: with(name, value),
                ..Action::default()
            };
            let got = request_of(&action, &Command::default()).expect("valid");
            assert_eq!(got.as_ref(), Some(&want), "{name}");
        }
        let command = Command {
            platform: with("simulator", value),
            ..Command::default()
        };
        assert_eq!(
            request_of(&Action::default(), &command).expect("valid"),
            Some(want)
        );
        let none = Action {
            platform: with("xcode", "16C5032a"),
            ..Action::default()
        };
        assert_eq!(request_of(&none, &Command::default()).expect("none"), None);
        for bad in ["", "iPhone 16", "iPhone 16,", ",iOS-18-4", "a,b,c"] {
            let action = Action {
                platform: with("simulator", bad),
                ..Action::default()
            };
            // Read through Debug: a pattern guard here would be a branch never taken.
            let got = format!("{:?}", request_of(&action, &Command::default()));
            assert!(got.starts_with("Err(Invalid("), "{bad:?}: {got}");
            assert!(got.contains(PROPERTY), "{bad:?}: {got}");
        }
    }

    /// Catches: a device of another runtime missed (the list is grouped by runtime), an
    /// entry whose UDID is not one taken for a device, and a list that is not one read
    /// as empty (the end would delete nothing and say nothing).
    #[test]
    fn the_device_list_is_every_runtimes_devices() {
        let json = br#"{"devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-18-4": [
                {"name": "kbf-lease-1-1", "udid": "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"},
                {"name": "odd", "udid": "not-a-udid"},
                {"udid": "1A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
                {"name": "iPhone 17", "udid": "2A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9"}
            ],
            "empty": "not a list"
        }}"#;
        assert_eq!(
            devices_in(json).expect("a list"),
            [
                (
                    "kbf-lease-1-1".to_owned(),
                    "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9".to_owned()
                ),
                (
                    "iPhone 17".to_owned(),
                    "2A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9".to_owned()
                ),
            ]
        );
        for bad in [&b"{}"[..], b"{\"devices\": []}", b"not JSON"] {
            assert!(devices_in(bad).is_err(), "{}", String::from_utf8_lossy(bad));
        }
    }

    /// Catches: a device another listing names taken for the lease's (a match on part
    /// of the UDID, or on any field), one Xcode marks unavailable taken for listed
    /// (`xcodebuild` would not use it), and a listing that is not an array of devices
    /// (`xcdevice` printing a warning) taken for one that names the device.
    #[test]
    fn a_listing_names_the_device_by_its_identifier() {
        let udid = "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9";
        let listed = |json: String| lists(json.as_bytes(), udid);
        assert!(listed(format!(
            r#"[{{"identifier": "x"}}, {{"identifier": "{udid}"}}]"#
        )));
        assert!(listed(format!(
            r#"[{{"identifier": "{udid}", "available": true}}]"#
        )));
        for not in [
            "[]".to_owned(),
            format!(r#"[{{"identifier": "{udid}", "available": false}}]"#),
            format!(r#"[{{"identifier": "{udid}0"}}]"#),
            format!(r#"[{{"name": "{udid}"}}]"#),
            format!(r#"{{"identifier": "{udid}"}}"#),
            format!("warning: {udid}"),
        ] {
            assert!(!listed(not.clone()), "{not}");
        }
    }

    /// Catches: a destination another simulator's line names taken for the device (a
    /// match on part of the id, or on the name), one under "Ineligible destinations"
    /// taken for available, the placeholders every run prints counted as simulators,
    /// and a UDID in lower case missed.
    #[test]
    fn a_destination_is_listed_by_its_whole_id_above_the_ineligible_heading() {
        let udid = "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9";
        let line =
            |id: &str| format!("\t\t{{ platform:iOS Simulator, id:{id}, OS:18.4, name:{udid} }}\n");
        let placeholder =
            line("dvtdevice-DVTiOSDeviceSimulatorPlaceholder-iphonesimulator:placeholder");
        let other = line("1A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9");
        let ineligible = "\n\tIneligible destinations for the \"X\" scheme:\n";
        assert_eq!(destinations("", udid), (false, 0));
        assert_eq!(
            destinations(&format!("{placeholder}{other}"), udid),
            (false, 1)
        );
        assert_eq!(
            destinations(&format!("{placeholder}{other}{}", line(udid)), udid),
            (true, 2)
        );
        assert_eq!(
            destinations(&format!("{other}{}", line(&udid.to_lowercase())), udid),
            (true, 2)
        );
        assert_eq!(
            destinations(&format!("{other}{ineligible}{}", line(udid)), udid),
            (false, 1)
        );
        assert_eq!(destinations(&line(&format!("{udid}0")), udid), (false, 0));
    }

    /// Catches: a name that is not a UDID taken for a device (the set's own
    /// `device_set.plist`, a file the action left), so the end would ask `simctl` to
    /// delete it, or a UDID in lower case missed.
    #[test]
    fn udids_are_8_4_4_4_12_hex_digits() {
        for udid in [
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9",
            "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9",
        ] {
            assert!(is_udid(udid), "{udid}");
        }
        for not in [
            "",
            "device_set.plist",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8F9-0",
            "0A1B2C3D4-E5F-6071-8293-A4B5C6D7E8F9",
            "0A1B2C3D-4E5F-6071-8293-A4B5C6D7E8FG",
        ] {
            assert!(!is_udid(not), "{not}");
        }
    }
}
