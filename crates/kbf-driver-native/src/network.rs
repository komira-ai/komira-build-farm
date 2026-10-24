//! Whether an action may use the network, and how the driver keeps it off.
//!
//! An action has the network unless it turns it off with the platform property
//! `network`: absent, `on` or `standard` gives it the network (the internet and the
//! LAN); `off` or `none` keeps it off (loopback still works). Anything else is the
//! client's error.
//!
//! On macOS every action runs under `sandbox-exec`. An action with `network=off`
//! gets [`NO_NETWORK_PROFILE`], which denies every network operation except loopback
//! and Unix sockets (the profile Bazel's macOS sandbox uses); one with the network gets
//! [`BASE_PROFILE`]. Both keep the action from handing work to launchd, which would run
//! it outside the sandbox and outside the action's process tree, and both deny every
//! file write outside the lease directory, `/dev` and the few names in the daemon
//! user's temporary folder that macOS tools write whatever `TMPDIR` says
//! ([`crate::user_folders`]), and every preference write through `cfprefsd`
//! ([`BASE_PROFILE`]): the action's
//! home, temporary and cache directories are inside the lease (`crate::home`), so a
//! tool that turns its own sandbox off (`swift build --disable-sandbox`) still writes
//! nowhere else.
//! Where `sandbox-exec` is missing, and on Linux, nothing is enforced: the action runs
//! with the node's network. Which of the two a node has is reported as the capability
//! `network_isolation` (`sandbox-exec` or `none`), so nothing about it is hidden.
//!
//! Every profile carries [`NO_DEVICE_RULES`], after the network rules and before the
//! lease's write rules. It keeps an action away from the
//! services that reach an iOS device on the node: the CoreDevice mach services
//! (`devicectl`'s `CoreDeviceService` among them), `remoted` and the `usbmuxd`
//! socket. No lease has a device yet, so no action may reach them
//! (`docs/design/ios-devices.md`, section 6.1).
//!
//! What the sandbox does not cover:
//! - Unix sockets are allowed, any of them, so a local service that talks to the
//!   network on the action's behalf is reachable. The plainest is the system's
//!   resolver (`mDNSResponder`): DNS lookups still go out, and a name an action looks
//!   up can carry data out of the node. Loopback services likewise.
//! - With `network=off`, a connect to the node's own non-loopback address is still
//!   allowed, as `tests/network.rs` saw on the macOS runner, so a service listening on
//!   the node's LAN address is reachable.
//! - Other mach and XPC services stay reachable (`(allow default)`), but for those
//!   [`NO_DEVICE_RULES`] names, Apple Events
//!   among them (`osascript`, the `appleevent-send` operation): an action can ask
//!   another application, such as Terminal or Finder, to run something for it outside
//!   the sandbox, where the user's session allows Apple Events.
//! - Anything else the daemon's user can schedule (a `LaunchAgents` plist that runs at
//!   its next login, `at`, `cron`, a loopback `ssh`). A per-lease user is the
//!   follow-up that closes this class.
//! - A program that itself uses `sandbox-exec` (macOS refuses a sandbox inside a
//!   sandbox) fails under the driver; that now includes actions with the network.
//! - Reads are not limited: an action reads what the daemon's user can, the node's key
//!   among it. Writes through a descriptor the action did not open itself (its stdout
//!   and stderr files) are not checked by path.
//! - A tool that insists on writing outside the lease (the per-user folders under
//!   `/var/folders` beyond the names [`crate::user_folders`] opens, `~` of the daemon's
//!   user named by absolute path or found through the user database, as SwiftPM and
//!   `xcodebuild` find `~/Library/Caches`, `/tmp`) fails; the action must point it
//!   into the lease (`HOME`, `TMPDIR`, a cache path flag such as `swift build
//!   --cache-path`).
//! - `xcodebuild` resolving a Swift package runs each manifest under SwiftPM's own
//!   `sandbox-exec`, which cannot nest in this sandbox, unless the Mac's user default
//!   `IDEPackageSupportDisableManifestSandbox` is on (`kbf-mac-provision`'s
//!   `xcode_disable_manifest_sandbox`; `swift build` has `--disable-sandbox`).
//!   The driver sets no preference itself: `user-preference-write` is denied.

use std::path::{Path, PathBuf};

use kbf_daemon::RuntimeError;
use kbf_proto::reapi::{Action, Command, Platform};

/// The platform property an action asks for the network with.
pub const PROPERTY: &str = "network";

/// The node report key that says how the network is kept off.
pub const CAPABILITY: &str = "network_isolation";

/// Where macOS keeps `sandbox-exec`.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The profile parameter that names the lease directory (`-D KBF_LEASE=<path>`), as
/// its real path: the sandbox compares the paths of the files written, with every link
/// resolved (`/var` is `/private/var`).
pub const LEASE_PARAM: &str = "KBF_LEASE";

/// The sandbox profile every action runs under: everything allowed but handing work
/// to launchd, which would run it outside the sandbox and outside the action's process
/// tree, and writing a file outside the lease directory ([`LEASE_PARAM`]) and `/dev`
/// (`/dev/null`, `/dev/tty`, `/dev/fd/<n>`). The driver appends the rules for the user
/// folders ([`crate::user_folders::UserFolders::rules`]) to this and to
/// [`NO_NETWORK_PROFILE`]. `lsopen` is opening an application or
/// document through Launch Services (`open`); `job-creation` is giving launchd a job
/// (`launchctl submit`, `load`, `bootstrap`); `file-write*` is every write operation:
/// create, write, unlink, rename, mode, flags, ACLs, extended attributes, times.
/// `user-preference-write` is asking `cfprefsd` to write a preference (`defaults
/// write`, `CFPreferences`): the daemon writes the user's `~/Library/Preferences`, not
/// the action, so `file-write*` does not cover it, and without the rule one action
/// leaves settings for the next (`tests/sandbox.rs` saw it on the macOS runner).
/// A hard link from inside the lease to a file outside it is already refused by these
/// rules, as the same test shows; there is no `file-link` rule.
///
/// Launchd already refuses a job from any sandboxed process, `(allow default)` alone
/// included, as `tests/launchd.rs` shows on the macOS runner (and the defects that
/// dropped `(deny job-creation)` stayed green). The rule stays as defence in depth: it
/// keeps the refusal in this profile rather than in launchd's current behaviour.
/// `(deny lsopen)` is load-bearing: without it an action opens an application, which
/// Launch Services starts outside the sandbox and outside the lease's process tree
/// (`tests/launchd.rs`): it writes anywhere the daemon's user can, has the network,
/// and outlives the lease. Naming the application does not make that narrower, since
/// an action can link an application of its own inside its lease and open that. So
/// macOS UI tests do not run under this driver: `xcodebuild` starts the UI-test
/// runner, an application built from the action's own sources, through Launch
/// Services, and on the macOS runner the rule stopped it (error -54,
/// `docs/spikes/macos-p0.md`). Where they run is a decision still open
/// (`docs/design/macos-vms.md`, section 3.2, puts them in a VM lease).
pub const BASE_PROFILE: &str = "(version 1)\n\
(allow default)\n\
(deny job-creation)\n\
(deny lsopen)\n\
(deny user-preference-write)\n\
(deny file-write*)\n\
(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))\n";

/// The sandbox profile for an action without the network: [`BASE_PROFILE`], and no
/// network operation but to loopback and Unix sockets.
pub const NO_NETWORK_PROFILE: &str = "(version 1)\n\
(allow default)\n\
(deny job-creation)\n\
(deny lsopen)\n\
(deny user-preference-write)\n\
(deny file-write*)\n\
(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))\n\
(deny network*)\n\
(allow network-inbound (local ip \"localhost:*\"))\n\
(allow network* (remote ip \"localhost:*\"))\n\
(allow network* (remote unix-socket))\n";

/// The data-vault folders under `/private/var/protected` where a GPU framework's first
/// use in a process of a uid leaves files (analytics and trust stores, one folder per
/// uid). They are OS-owned and out of the clean's scope (it has no Full Disk Access and
/// neither lists nor removes in them). A lease may not read or write them anyway, which
/// costs nothing: system daemons write there for the uid on its behalf, and a later
/// lease of the same uid would read what an earlier one left. The slot profile shuts
/// them ([`SLOT_PROFILE`]).
pub const PROTECTED_STORES: [&str; 2] = [
    "/private/var/protected/sfanalytics",
    "/private/var/protected/trustd/private",
];

/// The slot profile's deny of [`PROTECTED_STORES`], one line (the canary
/// `read-protected` stands for it); a macro so that `concat!` can build the profiles from
/// it. Last among the write rules: a later rule wins in a sandbox profile.
macro_rules! protected_deny {
    () => {
        "(deny file-read* file-write* (subpath \"/private/var/protected/sfanalytics\") (subpath \"/private/var/protected/trustd/private\"))"
    };
}

/// The deny line of [`PROTECTED_STORES`] in the slot profiles.
pub const PROTECTED_DENY: &str = protected_deny!();

/// The profile of an action that runs as a slot user of the lease-user pool
/// ([`crate::slots`]): [`BASE_PROFILE`] without its `job-creation` and
/// `user-preference-write` denies, and with `mach-register` and `file-mount` denied
/// besides. Those two denies kept one lease from leaving work or settings for the next
/// in a uid every lease shared; a slot's uid is its own, its preferences are in its own
/// home, and what it hands launchd runs as its uid, which the end of the lease kills
/// (docs/design/macos-end-state.md, section 1, "Sandbox"). The write rule stays, as a
/// hermeticity aid.
///
/// Plus the deny of [`PROTECTED_STORES`], read and write, after the write allows: the
/// clean does not touch those folders, so a lease may not either (docs/design/macos-end-state.md,
/// "GPU state"). The canary `read-protected` stands for it.
///
/// Three denies shut what the clean does not reach (each was open without the sandbox,
/// closed with the rule and open again with the rule removed, on a hosted macOS runner;
/// cargo, clang, swiftc, git, a Python venv and `swift build --disable-sandbox` ran
/// under them there once; no kept test runs them): `mach-register` is checking in a
/// Mach service name with launchd (`bootstrap_check_in`), which would give a service
/// that could outlive the action's process tree; `lsopen` is opening an application through Launch Services, which
/// starts it outside the sandbox (see [`BASE_PROFILE`]); `file-mount` is mounting a
/// file system on a directory (`hdiutil attach -mountpoint`). A slot's canaries
/// ([`crate::slots::canary`]) try each. Left open on purpose: `job-creation`, since
/// launchd refuses a job from any sandboxed process, `(allow default)` alone included,
/// so a canary asserts that refusal instead of a rule asserting it; `ipc-posix*`,
/// whose denial breaks Python's `multiprocessing` and `xcodebuild`; and a `hdiutil
/// attach` with no mount point, which still mounts under `/Volumes` through
/// `diskimages-helper` even with `file-mount` denied (the clean's unmount and the
/// helper's death remove it).
pub const SLOT_PROFILE: &str = concat!(
    "(version 1)\n",
    "(allow default)\n",
    "(deny mach-register)\n",
    "(deny lsopen)\n",
    "(deny file-mount)\n",
    "(deny file-write*)\n",
    "(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))\n",
    protected_deny!(),
    "\n"
);

/// [`SLOT_PROFILE`] for an action without the network: [`NO_NETWORK_PROFILE`] with the
/// same denies as [`SLOT_PROFILE`].
pub const SLOT_NO_NETWORK_PROFILE: &str = concat!(
    "(version 1)\n",
    "(allow default)\n",
    "(deny mach-register)\n",
    "(deny lsopen)\n",
    "(deny file-mount)\n",
    "(deny file-write*)\n",
    "(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))\n",
    protected_deny!(),
    "\n",
    "(deny network*)\n",
    "(allow network-inbound (local ip \"localhost:*\"))\n",
    "(allow network* (remote ip \"localhost:*\"))\n",
    "(allow network* (remote unix-socket))\n"
);

/// The profile of a slot user's action under `network`: [`SLOT_PROFILE`] or
/// [`SLOT_NO_NETWORK_PROFILE`].
#[must_use]
pub const fn slot_profile(network: Network) -> &'static str {
    match network {
        Network::Off => SLOT_NO_NETWORK_PROFILE,
        Network::On => SLOT_PROFILE,
    }
}

/// The rule that lets a slot user write its own `home` and `/var/folders` entry
/// (`folders`), each by its real path.
///
/// # Errors
/// A path holds a character a sandbox profile string cannot carry.
pub fn slot_write_rules(home: &Path, folders: &Path) -> std::io::Result<String> {
    Ok(format!(
        "(allow file-write* (subpath {}) (subpath {}))\n",
        quoted(home)?,
        quoted(folders)?
    ))
}

/// `path` as a sandbox profile string literal.
fn quoted(path: &Path) -> std::io::Result<String> {
    let text = path.to_str().unwrap_or("\"");
    if text.contains(['"', '\\']) {
        return Err(std::io::Error::other(format!(
            "{} cannot be named in a sandbox profile",
            path.display()
        )));
    }
    Ok(format!("\"{text}\""))
}

/// A change made to the generated profile, to see a canary go red on a Mac: for the
/// tests of `tests/slot_canary.rs` alone, never set in production.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WeakenedProfile {
    /// Drops every copy of this line.
    Drop(&'static str),
    /// Drops every copy of each of these lines.
    DropAll(&'static [&'static str]),
    /// Adds this line at the end of the profile constant (before the device and home
    /// rules).
    Add(&'static str),
}

impl WeakenedProfile {
    /// `profile` with the change made.
    #[must_use]
    pub fn apply(self, profile: &str) -> String {
        match self {
            Self::Drop(line) => profile
                .split_inclusive('\n')
                .filter(|l| l.trim() != line)
                .collect(),
            Self::DropAll(lines) => profile
                .split_inclusive('\n')
                .filter(|l| !lines.contains(&l.trim()))
                .collect(),
            Self::Add(line) => format!("{profile}{line}\n"),
        }
    }
}

/// The rules every profile carries (after the network rules): no lookup of a
/// CoreDevice mach service or XPC service (`com.apple.CoreDevice.CoreDeviceService`,
/// which `devicectl` asks for everything, `com.apple.CoreDevice.remotepairingd`,
/// `com.apple.CoreDevice.CoreDeviceDDIUpdaterService` and the rest of the prefix), none
/// of `remoted`'s (`com.apple.remoted`, its `.coredevice` and `.control` services and
/// the rest of the prefix: RemoteXPC, the tunnel CoreDevice talks to a device through),
/// and no connect to `usbmuxd`'s socket, which the sandbox names by its real path
/// (`/var/run` is a link to `/private/var/run`; a rule on the link's path matches
/// nothing). `tests/devices.rs` shows each refused on the macOS runner, and reached
/// without the sandbox, `remotepairingd`, `.coredevice` and `.control` among them; this
/// module's tests check that each mach rule is a `global-name-prefix` covering every
/// name listed here. Their place in the profile does
/// not decide: with the `usbmuxd` rule before [`NO_NETWORK_PROFILE`]'s allow for every
/// Unix socket (a defect), the runner still refused the connect. A lease that books a
/// device is to get a profile without these (`docs/design/ios-devices.md`, section
/// 6.1); none does yet.
pub const NO_DEVICE_RULES: &str = "(deny mach-lookup (global-name-prefix \"com.apple.CoreDevice.\"))\n\
(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))\n\
(deny network-outbound (literal \"/private/var/run/usbmuxd\"))\n";

/// What an action asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Network {
    Off,
    On,
}

/// The action's `network` property, from the Action's platform or, for clients older
/// than REAPI 2.2, the Command's.
///
/// # Errors
/// [`RuntimeError::Invalid`] for a value other than those the module lists.
#[allow(deprecated)]
pub fn network_of(action: &Action, command: &Command) -> Result<Network, RuntimeError> {
    let platform: Option<&Platform> = action.platform.as_ref().or(command.platform.as_ref());
    let value = platform
        .and_then(|p| p.properties.iter().find(|p| p.name == PROPERTY))
        .map(|p| p.value.as_str());
    match value {
        Some("off" | "none") => Ok(Network::Off),
        None | Some("on" | "standard") => Ok(Network::On),
        Some(other) => Err(RuntimeError::Invalid(format!(
            "platform property {PROPERTY}={other:?}: use on, standard, off or none"
        ))),
    }
}

/// How this node keeps the network off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Isolation {
    /// Through `sandbox-exec` at this path.
    Sandbox(PathBuf),
    /// Not at all: an action that asks for no network still has it.
    None,
}

impl Isolation {
    /// `sandbox-exec` at `program` if it is there, otherwise none.
    #[must_use]
    pub fn detect_at(program: &Path) -> Self {
        if program.is_file() {
            Self::Sandbox(program.to_owned())
        } else {
            Self::None
        }
    }

    /// This node's isolation: `sandbox-exec` on a Mac that has it.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn detect() -> Self {
        Self::detect_at(Path::new(SANDBOX_EXEC))
    }

    /// This node's isolation: none, off macOS.
    #[cfg(not(target_os = "macos"))]
    #[must_use]
    pub fn detect() -> Self {
        Self::None
    }

    /// The value of the [`CAPABILITY`] entry in the node report.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sandbox(_) => "sandbox-exec",
            Self::None => "none",
        }
    }

    /// The program and arguments that run `program` with `args` under `network`,
    /// writing only inside `lease` (the lease directory's real path), `/dev`, and what
    /// `rules` (appended to the profile) allows: the names in the user's folders the
    /// tools need ([`crate::user_folders::UserFolders::rules`]). The profile is the
    /// network's, then [`NO_DEVICE_RULES`], then `rules`.
    #[must_use]
    pub fn wrap(
        &self,
        network: Network,
        lease: &Path,
        rules: &str,
        program: PathBuf,
        args: &[String],
    ) -> (PathBuf, Vec<String>) {
        let profile = match network {
            Network::Off => NO_NETWORK_PROFILE,
            Network::On => BASE_PROFILE,
        };
        self.wrap_in(profile, lease, rules, program, args)
    }

    /// [`Self::wrap`] for an action that runs as a lease-user pool's slot user
    /// ([`crate::slots`]): the profile is [`SLOT_PROFILE`] or
    /// [`SLOT_NO_NETWORK_PROFILE`], and `rules` lets it write the slot user's own home
    /// and `/var/folders` entry.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn wrap_slot(
        &self,
        network: Network,
        lease: &Path,
        rules: &str,
        program: PathBuf,
        args: &[String],
    ) -> (PathBuf, Vec<String>) {
        self.wrap_in(slot_profile(network), lease, rules, program, args)
    }

    /// `program` with `args` under `profile`, then [`NO_DEVICE_RULES`] and `rules`.
    pub fn wrap_in(
        &self,
        profile: &str,
        lease: &Path,
        rules: &str,
        program: PathBuf,
        args: &[String],
    ) -> (PathBuf, Vec<String>) {
        let Self::Sandbox(sandbox) = self else {
            return (program, args.to_vec());
        };
        let mut wrapped = vec![
            "-D".to_owned(),
            format!("{LEASE_PARAM}={}", lease.to_string_lossy()),
            "-p".to_owned(),
            format!("{profile}{NO_DEVICE_RULES}{rules}"),
            program.to_string_lossy().into_owned(),
        ];
        wrapped.extend(args.iter().cloned());
        (sandbox.clone(), wrapped)
    }
}

#[cfg(test)]
mod tests {
    use kbf_proto::reapi::platform::Property;

    use super::*;

    /// Catches a slot profile that keeps a deny the pool made needless (a slot user's
    /// `defaults write` refused), lacks one it adds (`mach-register`, `file-mount`), or
    /// differs from the shared user's in anything else (a write rule, a network rule or
    /// the `lsopen` deny lost).
    #[cfg(target_os = "macos")]
    #[test]
    fn a_slot_profile_is_the_shared_one_without_two_denies_and_with_three_more() {
        let without = |profile: &str| {
            let gone = ["(deny job-creation)", "(deny user-preference-write)"];
            let mut kept: Vec<&str> = profile.lines().filter(|l| !gone.contains(l)).collect();
            assert_eq!(
                kept.len() + gone.len(),
                profile.lines().count(),
                "{profile}"
            );
            let at = kept
                .iter()
                .position(|l| *l == "(deny lsopen)")
                .expect("lsopen");
            kept.splice(at..at, ["(deny mach-register)"]);
            kept.splice(at + 2..at + 2, ["(deny file-mount)"]);
            let after = kept
                .iter()
                .position(|l| l.starts_with("(allow file-write*"))
                .expect("write allow");
            kept.splice(after + 1..after + 1, [PROTECTED_DENY]);
            kept.join("\n") + "\n"
        };
        assert_eq!(SLOT_PROFILE, without(BASE_PROFILE));
        assert_eq!(SLOT_NO_NETWORK_PROFILE, without(NO_NETWORK_PROFILE));
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        let lease = Path::new("/l");
        let args = ["a".to_owned()];
        for (network, profile) in [
            (Network::On, SLOT_PROFILE),
            (Network::Off, SLOT_NO_NETWORK_PROFILE),
        ] {
            let (program, wrapped) =
                sandbox.wrap_slot(network, lease, "(rule)\n", PathBuf::from("/p"), &args);
            assert_eq!(program, PathBuf::from(SANDBOX_EXEC));
            assert_eq!(wrapped[3], format!("{profile}{NO_DEVICE_RULES}(rule)\n"));
            assert_eq!(wrapped[4..], ["/p", "a"]);
        }
    }

    fn platform(value: &str) -> Option<Platform> {
        Some(Platform {
            properties: vec![Property {
                name: PROPERTY.to_owned(),
                value: value.to_owned(),
            }],
        })
    }

    /// Catches: the network off by default (the old rule), an unknown value taken as
    /// on or off instead of refused, the Command's deprecated platform ignored, and
    /// the Command's platform overriding the Action's.
    #[test]
    #[allow(deprecated)]
    fn the_network_is_on_unless_turned_off() {
        let none = (Action::default(), Command::default());
        assert_eq!(network_of(&none.0, &none.1).ok(), Some(Network::On));
        for (value, want) in [
            ("off", Network::Off),
            ("none", Network::Off),
            ("on", Network::On),
            ("standard", Network::On),
        ] {
            let action = Action {
                platform: platform(value),
                ..Action::default()
            };
            assert_eq!(network_of(&action, &none.1).ok(), Some(want), "{value}");
        }
        let bad = Action {
            platform: platform("yes"),
            ..Action::default()
        };
        assert!(matches!(
            network_of(&bad, &none.1),
            Err(RuntimeError::Invalid(_))
        ));
        // The Command's platform turns the network off (`off`, not `on`: `on` is also
        // the default, so a check with it passes when the Command is ignored).
        let command = Command {
            platform: platform("off"),
            ..Command::default()
        };
        assert_eq!(
            network_of(&Action::default(), &command).ok(),
            Some(Network::Off)
        );
        let action = Action {
            platform: platform("on"),
            ..Action::default()
        };
        assert_eq!(network_of(&action, &command).ok(), Some(Network::On));
    }

    /// The names the docs of [`NO_DEVICE_RULES`] list for each of its mach rules.
    const DEVICE_SERVICES: [(&str, &[&str]); 2] = [
        (
            "CoreDevice",
            &[
                "com.apple.CoreDevice.CoreDeviceService",
                "com.apple.CoreDevice.remotepairingd",
                "com.apple.CoreDevice.CoreDeviceDDIUpdaterService",
            ],
        ),
        (
            "remoted",
            &[
                "com.apple.remoted",
                "com.apple.remoted.coredevice",
                "com.apple.remoted.control",
            ],
        ),
    ];

    /// Catches a device rule narrowed from its prefix to one name (`(global-name
    /// "com.apple.CoreDevice.CoreDeviceService")`, `(global-name "com.apple.remoted")`)
    /// or to a longer prefix, which `devicectl` and `remotectl` alone do not notice: in
    /// the profile `wrap` builds, with the network off and on, each service's listed
    /// names must all start with the string of one `(deny mach-lookup
    /// (global-name-prefix ..))` rule, and no rule may allow a lookup back.
    #[test]
    fn each_device_rule_denies_a_whole_prefix() {
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        for network in [Network::Off, Network::On] {
            let (_, args) = sandbox.wrap(
                network,
                Path::new("/private/var/kbf/lease-1-2"),
                "",
                PathBuf::from("/bin/sh"),
                &[],
            );
            let profile = &args[3];
            let prefixes: Vec<&str> = profile
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("(deny mach-lookup (global-name-prefix \"")?
                        .strip_suffix("\"))")
                })
                .collect();
            for (service, names) in DEVICE_SERVICES {
                assert!(names.len() >= 2, "{service}: list two names or more");
                assert!(
                    prefixes
                        .iter()
                        .any(|prefix| names.iter().all(|name| name.starts_with(prefix))),
                    "{network:?}: no prefix rule covers every {service} name {names:?}:\n{profile}"
                );
            }
            assert!(
                !profile.contains("(allow mach-lookup"),
                "{network:?}: {profile}"
            );
        }
    }

    /// Catches a profile that loses one of its denies, or gets an allow that takes one
    /// back, with the network off and on and the user-folder rules appended:
    /// `(deny lsopen)` above all, without which an action opens an application outside
    /// the sandbox (`tests/launchd.rs`; macOS UI tests need it and stay out,
    /// `docs/spikes/macos-p0.md`), and every rule of [`NO_DEVICE_RULES`]. The appended
    /// rules may only allow file writes.
    #[test]
    fn every_profile_keeps_its_denies() {
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        let folders = crate::user_folders::UserFolders::new(
            PathBuf::from("/private/var/folders/ab/cd/T"),
            PathBuf::from("/private/var/folders/ab/cd/C"),
        )
        .expect("fits");
        let rules = folders.rules();
        let denies = [
            "(deny job-creation)",
            "(deny lsopen)",
            "(deny user-preference-write)",
            "(deny file-write*)",
        ];
        for network in [Network::Off, Network::On] {
            let (_, args) = sandbox.wrap(
                network,
                Path::new("/private/var/kbf/lease-1-2"),
                &rules,
                PathBuf::from("/bin/sh"),
                &[],
            );
            let profile = &args[3];
            let lines: Vec<&str> = profile.lines().collect();
            for deny in denies.iter().copied().chain(NO_DEVICE_RULES.lines()) {
                assert!(lines.contains(&deny), "{network:?}: no {deny}:\n{profile}");
            }
            for operation in [
                "lsopen",
                "job-creation",
                "user-preference-write",
                "mach-lookup",
            ] {
                assert!(
                    !profile.contains(&format!("(allow {operation}")),
                    "{network:?}: {operation} allowed:\n{profile}"
                );
            }
            let appended = &profile
                [profile.find(NO_DEVICE_RULES).expect("device rules") + NO_DEVICE_RULES.len()..];
            assert_eq!(appended, rules, "{network:?}");
            // Three asserts, not one `&&` chain: the coverage ratchet counts each
            // short-circuit a test never takes as a missed branch.
            assert!(appended.starts_with("(allow file-write*\n"), "{appended}");
            assert_eq!(appended.matches("(allow").count(), 1, "{appended}");
            assert!(!appended.contains("(deny"), "{appended}");
        }
    }

    /// Catches: the network denied to an action that asked for it, or allowed one that
    /// did not; an action with the network left unsandboxed (free to hand work to
    /// launchd); the device rules missing from either profile, or put before
    /// `(version 1)`, which must open a profile; the program or its arguments lost or
    /// reordered inside the wrapper; and a node without `sandbox-exec` claiming
    /// isolation.
    #[test]
    fn every_action_is_wrapped_and_only_the_network_differs() {
        let args = vec!["-c".to_owned(), "echo hi".to_owned()];
        let program = PathBuf::from("/bin/sh");
        let lease = Path::new("/private/var/kbf/lease-1-2");
        let param = "KBF_LEASE=/private/var/kbf/lease-1-2";
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        assert_eq!(sandbox.name(), "sandbox-exec");
        let rules = "(allow file-write* (literal \"/x\"))\n";
        let (wrapped, wrapped_args) =
            sandbox.wrap(Network::Off, lease, rules, program.clone(), &args);
        assert_eq!(wrapped, PathBuf::from(SANDBOX_EXEC));
        assert_eq!(
            wrapped_args,
            [
                "-D",
                param,
                "-p",
                &format!("{NO_NETWORK_PROFILE}{NO_DEVICE_RULES}{rules}"),
                "/bin/sh",
                "-c",
                "echo hi"
            ]
        );
        let (wrapped, wrapped_args) = sandbox.wrap(Network::On, lease, "", program.clone(), &args);
        assert_eq!(wrapped, PathBuf::from(SANDBOX_EXEC));
        assert_eq!(
            wrapped_args,
            [
                "-D",
                param,
                "-p",
                &format!("{BASE_PROFILE}{NO_DEVICE_RULES}"),
                "/bin/sh",
                "-c",
                "echo hi"
            ]
        );
        assert!(NO_NETWORK_PROFILE.starts_with(BASE_PROFILE));
        assert!(BASE_PROFILE.contains(&format!("(param \"{LEASE_PARAM}\")")));
        assert_eq!(Isolation::None.name(), "none");
        assert_eq!(
            Isolation::None.wrap(Network::Off, lease, rules, program.clone(), &args),
            (program, args)
        );
        assert_eq!(
            Isolation::detect_at(Path::new("/nonexistent/sandbox-exec")),
            Isolation::None
        );
        let here = std::env::current_exe().expect("test binary");
        assert_eq!(
            Isolation::detect_at(&here),
            Isolation::Sandbox(here.clone())
        );
        #[cfg(target_os = "linux")]
        assert_eq!(Isolation::detect(), Isolation::None);
    }

    /// The profile a slot user's lease runs under, as the driver generates it for
    /// `network`: the text after `-p` of the sandbox command line.
    fn generated(network: Network, weakened: Option<WeakenedProfile>) -> String {
        let rules =
            slot_write_rules(Path::new("/h/slot"), Path::new("/v/f/xx/yyyy")).expect("rules");
        let text = weakened.map_or_else(
            || slot_profile(network).to_owned(),
            |w| w.apply(slot_profile(network)),
        );
        let sandbox = Isolation::Sandbox(PathBuf::from(SANDBOX_EXEC));
        let (_, args) = sandbox.wrap_in(&text, Path::new("/lease"), &rules, "p".into(), &[]);
        let at = args.iter().position(|a| a == "-p").expect("-p");
        args[at + 1].clone()
    }

    /// Every rule the slot profile is meant to carry, one row each: the line, and
    /// whether it is there only with the network off.
    const SLOT_RULES: &[(&str, bool)] = &[
        ("(version 1)", false),
        ("(allow default)", false),
        ("(deny mach-register)", false),
        ("(deny lsopen)", false),
        ("(deny file-mount)", false),
        ("(deny file-write*)", false),
        (
            "(allow file-write* (subpath (param \"KBF_LEASE\")) (subpath \"/dev\"))",
            false,
        ),
        (PROTECTED_DENY, false),
        (
            "(allow file-write* (subpath \"/h/slot\") (subpath \"/v/f/xx/yyyy\"))",
            false,
        ),
        ("(deny network*)", true),
        ("(allow network-inbound (local ip \"localhost:*\"))", true),
        ("(allow network* (remote ip \"localhost:*\"))", true),
        ("(allow network* (remote unix-socket))", true),
        (
            "(deny mach-lookup (global-name-prefix \"com.apple.CoreDevice.\"))",
            false,
        ),
        (
            "(deny mach-lookup (global-name-prefix \"com.apple.remoted\"))",
            false,
        ),
        (
            "(deny network-outbound (literal \"/private/var/run/usbmuxd\"))",
            false,
        ),
    ];

    /// The rows of [`SLOT_RULES`] that the profile for `network` must hold.
    fn wanted(network: Network) -> impl Iterator<Item = &'static str> {
        SLOT_RULES
            .iter()
            .filter(move |(_, off_only)| !off_only || network == Network::Off)
            .map(|(rule, _)| *rule)
    }

    /// Catches a generated slot profile that lacks a deny or allow the code means it to
    /// carry (a dropped `(deny file-write*)` would let a pool user write anywhere, and
    /// only the macOS canary would notice), on Linux and in every pull request. Weakening
    /// is caught: deleting the `(deny file-write*)` line from `SLOT_PROFILE` fails the
    /// `On` and `Off` rows for it; dropping it from `SLOT_NO_NETWORK_PROFILE` alone
    /// fails `Off`.
    #[test]
    fn the_slot_profile_holds_every_rule_it_means_to() {
        for network in [Network::On, Network::Off] {
            let text = generated(network, None);
            let lines: Vec<&str> = text.lines().collect();
            for rule in wanted(network) {
                assert!(
                    lines.contains(&rule),
                    "{network:?}: no line {rule:?} in\n{text}"
                );
            }
            if network == Network::On {
                assert!(!text.contains("(deny network*)"), "{text}");
            }
        }
    }

    /// Catches a slot profile that denies what the pool leaves open on purpose:
    /// `job-creation` (launchd refuses a sandboxed job anyway, and the bootstrap canary
    /// asserts that), `ipc-posix*` (it breaks Python's `multiprocessing` and
    /// `xcodebuild`) and `user-preference-write` (a slot's preferences are its own).
    #[test]
    fn the_slot_profile_leaves_three_operations_open() {
        for network in [Network::On, Network::Off] {
            let text = generated(network, None);
            for open in ["job-creation", "ipc-posix", "user-preference-write"] {
                assert!(!text.contains(open), "{network:?}: {open} in\n{text}");
            }
        }
    }

    /// Catches the check above passing when a rule is missing (a test that cannot go
    /// red): drops each row in turn with [`WeakenedProfile`] and sees the rule found
    /// missing, and that the weakening removes that line alone.
    #[test]
    fn dropping_any_one_rule_is_seen() {
        for network in [Network::On, Network::Off] {
            let whole = generated(network, None);
            // The weakening edits the profile constants; the device and home rules are
            // appended after it.
            let own = |r: &&str| slot_profile(network).lines().any(|l| l == *r);
            for rule in wanted(network).filter(|r| *r != "(version 1)").filter(own) {
                let changed = generated(network, Some(WeakenedProfile::Drop(rule)));
                let lines: Vec<&str> = changed.lines().collect();
                assert!(!lines.contains(&rule), "{rule} is still there");
                let before = whole.lines().filter(|l| *l == rule).count();
                assert_eq!(whole.lines().count() - lines.len(), before, "{rule}");
            }
        }
    }

    /// Catches a protected-store deny that does not shut what the canary
    /// names (it lists one store, or reads and not writes), or one a later allow undoes (a
    /// later rule wins in a sandbox profile, so it must come after the lease's write allow):
    /// the line names both stores of [`PROTECTED_STORES`] and both operations, and follows
    /// the allow of the lease directory, in both profiles.
    #[test]
    fn the_protected_stores_are_denied_read_and_write_after_the_lease_allow() {
        assert_eq!(PROTECTED_STORES.len(), 2);
        for store in PROTECTED_STORES {
            assert!(
                PROTECTED_DENY.contains(&format!("(subpath \"{store}\")")),
                "{store}"
            );
        }
        assert!(PROTECTED_DENY.starts_with("(deny file-read* file-write* "));
        for network in [Network::On, Network::Off] {
            let text = generated(network, None);
            let lines: Vec<&str> = text.lines().collect();
            let at = |want: &str| lines.iter().position(|l| *l == want).expect(want);
            let lease_allow = lines
                .iter()
                .position(|l| l.starts_with("(allow file-write* (subpath (param"))
                .expect("lease allow");
            assert!(at(PROTECTED_DENY) > lease_allow, "{network:?}:\n{text}");
            assert_eq!(lines.iter().filter(|l| **l == PROTECTED_DENY).count(), 1);
        }
    }

    /// Catches a profile change that does not change the profile as it says (an added rule that
    /// is lost, or a drop that takes a neighbour).
    #[test]
    fn a_weakened_profile_adds_or_drops_exactly_its_line() {
        let base = "(a)\n(b)\n(a)\n";
        assert_eq!(WeakenedProfile::Drop("(a)").apply(base), "(b)\n");
        // A whole line, not a substring or a prefix of one.
        let near = "(a)\n(a) (b)\n  (a)  \n((a))\n(ab)\n";
        assert_eq!(
            WeakenedProfile::Drop("(a)").apply(near),
            "(a) (b)\n((a))\n(ab)\n"
        );
        assert_eq!(
            WeakenedProfile::DropAll(&["(a)", "(c)"]).apply("(a)\n(b)\n(c)\n(a)\n"),
            "(b)\n"
        );
        assert_eq!(
            WeakenedProfile::DropAll(&["(a)"]).apply(near),
            "(a) (b)\n((a))\n(ab)\n"
        );
        assert_eq!(
            WeakenedProfile::Add("(c)").apply(base),
            "(a)\n(b)\n(a)\n(c)\n"
        );
    }

    /// Catches a canary that names a deny the profile does not hold (so it would test a
    /// door nothing shuts, or a renamed rule that no canary follows).
    #[test]
    fn every_canary_names_a_rule_of_the_profile() {
        for canary in crate::slots::canary::CANARIES {
            for network in [Network::On, Network::Off] {
                assert!(
                    generated(network, None).lines().any(|l| l == canary.deny),
                    "{}: {}",
                    canary.name,
                    canary.deny
                );
            }
        }
    }

    /// Catches a write rule built from a path the profile cannot quote (a quote or
    /// backslash would end the string early and let the rest be read as rules).
    #[test]
    fn a_path_that_cannot_be_quoted_makes_no_rule() {
        assert!(slot_write_rules(Path::new("/h\"x"), Path::new("/f")).is_err());
        assert!(slot_write_rules(Path::new("/h"), Path::new("/f\\x")).is_err());
        let ok = slot_write_rules(Path::new("/h"), Path::new("/f")).expect("plain paths");
        assert_eq!(
            ok,
            "(allow file-write* (subpath \"/h\") (subpath \"/f\"))\n"
        );
    }
}
