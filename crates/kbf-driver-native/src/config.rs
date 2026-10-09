//! Where and how the native driver runs actions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_outputs::OutputLimits;

use crate::network::Isolation;
use crate::user_folders::{LEFTOVER_AGE, UserFolders};
use crate::xcode;

/// How much memory one lease's processes may hold together before they are killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// The limit is this percentage of what the lease booked...
    pub percent: u64,
    /// ...plus this many bytes.
    pub headroom_bytes: u64,
}

impl MemoryPolicy {
    /// The container driver's soft limit made hard: 150 % of the booking plus 512 MiB.
    pub const DEFAULT: Self = Self {
        percent: 150,
        headroom_bytes: 512 << 20,
    };

    /// The limit for a lease that booked `memory_bytes`; `None` (no limit) when nothing
    /// was booked, as the container driver does.
    #[must_use]
    pub fn limit(&self, memory_bytes: u64) -> Option<u64> {
        (memory_bytes > 0).then(|| {
            u64::try_from(u128::from(memory_bytes) * u128::from(self.percent) / 100)
                .unwrap_or(u64::MAX)
                .saturating_add(self.headroom_bytes)
        })
    }
}

impl Default for MemoryPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where and how the native driver runs actions.
#[derive(Clone, Debug)]
pub struct NativeConfig {
    /// The directory each lease's directory is made in.
    pub scratch: PathBuf,
    /// The timeout of an action that names none.
    pub default_timeout: Duration,
    /// How much output one action may leave, stdout and stderr included.
    pub outputs: OutputLimits,
    /// The per-lease memory limit.
    pub memory: MemoryPolicy,
    /// How often the action's memory is measured (and its process tree refreshed).
    pub poll: Duration,
    /// How long ending an action's processes may take before the survivors fail the
    /// lease.
    pub kill_wait: Duration,
    /// How the network is kept off.
    pub isolation: Isolation,
    /// The Xcodes an action may name, by build (`16C5032a`), each as the path its
    /// `DEVELOPER_DIR` takes (`.../Xcode.app/Contents/Developer`); see
    /// [`crate::xcode::discover`].
    pub xcodes: BTreeMap<String, PathBuf>,
    /// The daemon user's temporary and cache folders, where the sandbox lets an action
    /// write the few names macOS tools use there whatever `TMPDIR` says
    /// ([`crate::user_folders`]); `None` off macOS.
    pub user_folders: Option<UserFolders>,
    /// How old a leftover in [`Self::user_folders`] must be before a sweep removes it.
    pub leftover_age: Duration,
}

impl NativeConfig {
    /// A configuration with a one hour default timeout, the default output limits and
    /// memory policy, a 250 ms poll, a 5 s kill wait, this node's isolation and user
    /// folders, leftovers swept after [`LEFTOVER_AGE`], and no Xcode (the daemon fills
    /// [`Self::xcodes`] with [`Self::find_xcodes`]).
    #[must_use]
    pub fn new(scratch: PathBuf) -> Self {
        Self {
            scratch,
            default_timeout: Duration::from_secs(3600),
            outputs: OutputLimits::DEFAULT,
            memory: MemoryPolicy::DEFAULT,
            poll: Duration::from_millis(250),
            kill_wait: Duration::from_secs(5),
            isolation: Isolation::detect(),
            xcodes: BTreeMap::new(),
            user_folders: UserFolders::detect(),
            leftover_age: LEFTOVER_AGE,
        }
    }

    /// Fills [`Self::xcodes`] with the Xcodes in `apps` that answer within `within`
    /// ([`xcode::discover`]), after removing `xcrun`'s cache from the user folders
    /// ([`UserFolders::forget_xcrun_cache`]): the daemon runs these Xcodes' tools
    /// outside the sandbox, and leases can write that cache.
    pub fn find_xcodes(&mut self, apps: &Path, within: Duration) {
        if let Some(folders) = &self.user_folders {
            folders.forget_xcrun_cache();
        }
        self.xcodes = xcode::discover(apps, Path::new(xcode::XCODEBUILD), within);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches: a limit for an unbooked lease (which would kill everything that books
    /// nothing at the headroom), a percentage applied twice or not at all, and an
    /// overflow on a huge booking.
    #[test]
    fn the_limit_is_a_percentage_of_the_booking_plus_headroom() {
        let policy = MemoryPolicy::DEFAULT;
        assert_eq!(policy.limit(0), None);
        assert_eq!(policy.limit(1 << 30), Some((3 << 29) + (512 << 20)));
        assert_eq!(policy.limit(u64::MAX), Some(u64::MAX));
        let tight = MemoryPolicy {
            percent: 100,
            headroom_bytes: 0,
        };
        assert_eq!(tight.limit(64 << 20), Some(64 << 20));
        assert_eq!(MemoryPolicy::default(), policy);
    }

    /// Catches Xcodes asked for their build (outside the sandbox) while `xcrun`'s
    /// cache a lease could have written is still there: the fake `xcodebuild` answers
    /// a build that says which it saw.
    #[test]
    fn xcodes_are_found_after_the_xcrun_cache_is_gone() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::current_exe()
            .expect("test binary")
            .parent()
            .expect("deps")
            .join("kbf-driver-native-unit")
            .join(format!("config-xcodes-{}", std::process::id()));
        let _ = kbf_outputs::remove_tree(&dir);
        let (temp, cache) = (dir.join("T"), dir.join("C"));
        std::fs::create_dir_all(&temp).expect("T");
        std::fs::create_dir_all(&cache).expect("C");
        let apps = dir.join("Applications");
        let program = apps
            .join("Xcode.app/Contents/Developer")
            .join(xcode::XCODEBUILD);
        std::fs::create_dir_all(program.parent().expect("bin")).expect("bin");
        let script = format!(
            "#!/bin/sh\n\
             if [ -e '{}/xcrun_db' ]; then echo 'Build version CACHE'; \
             else echo 'Build version 1A1'; fi\n",
            temp.display()
        );
        std::fs::write(&program, script).expect("script");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::write(temp.join("xcrun_db"), b"a lease's").expect("cache");
        let mut config = NativeConfig::new(dir.join("leases"));
        config.user_folders = UserFolders::new(temp, cache);
        config.find_xcodes(&apps, Duration::from_secs(5));
        assert_eq!(config.xcodes.keys().collect::<Vec<_>>(), ["1A1"]);
        // Without user folders (off macOS) the Xcodes are found all the same.
        config.user_folders = None;
        config.xcodes.clear();
        config.find_xcodes(&apps, Duration::from_secs(5));
        assert_eq!(config.xcodes.keys().collect::<Vec<_>>(), ["1A1"]);
        kbf_outputs::remove_tree(&dir).expect("clean");
    }
}
