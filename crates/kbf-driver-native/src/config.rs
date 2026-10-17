//! Where and how the native driver runs actions.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::cache::Cache;
use kbf_daemon::trees::Trees;
use kbf_outputs::OutputLimits;

use crate::denials::LogReader;
use crate::network::Isolation;
use crate::simulator::Listing;
use crate::slots::Pool;
use crate::user_folders::{LEFTOVER_AGE, UserFolders};

/// How much memory one lease's processes may hold together before they are killed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryPolicy {
    /// The limit is this percentage of what the lease booked...
    pub percent: u64,
    /// ...plus this many bytes.
    pub headroom_bytes: u64,
}

impl MemoryPolicy {
    /// 150 % of the booking plus 512 MiB, the container driver's per-lease cap too.
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
    /// How long one lease's whole output upload (outputs, stdout and stderr) may take
    /// before the lease fails saying so ([`kbf_daemon::upload_within`]).
    pub upload_deadline: Duration,
    /// The per-lease memory limit.
    pub memory: MemoryPolicy,
    /// How often the action's memory is measured (and its process tree refreshed).
    pub poll: Duration,
    /// How long ending an action's processes may take before the survivors fail the
    /// lease.
    pub kill_wait: Duration,
    /// How long a lease's new simulator may take to be listed by Xcode, and how often
    /// the driver looks, before the action runs ([`crate::simulator`]).
    pub simulator_listing: Listing,
    /// How the network is kept off.
    pub isolation: Isolation,
    /// The Xcodes an action may name when the runtime starts, by build (`16C5032a`),
    /// each as the path its `DEVELOPER_DIR` takes (`.../Xcode.app/Contents/Developer`).
    /// The daemon leaves it empty and sets the ready ones of each survey with
    /// [`crate::NativeRuntime::apply_xcodes`] ([`crate::xcode_watch`]).
    pub xcodes: BTreeMap<String, PathBuf>,
    /// The daemon user's temporary and cache folders, where the sandbox lets an action
    /// write the few names macOS tools use there whatever `TMPDIR` says
    /// ([`crate::user_folders`]); `None` off macOS.
    pub user_folders: Option<UserFolders>,
    /// How old a leftover in [`Self::user_folders`] must be before a sweep removes it.
    pub leftover_age: Duration,
    /// The `xcrun` that runs `simctl` for the leases that ask for a simulator
    /// ([`crate::simulator`]).
    pub xcrun: PathBuf,
    /// The lease-user pool every action runs as, one user per slot ([`crate::slots`]):
    /// required of a daemon that runs as root, refused otherwise and off macOS.
    pub pool: Option<Pool>,
    /// The node's input cache: a lease's input files are clones of its entries
    /// (`kbf_daemon::tree::materialize_cached`) and only a blob it does not hold is
    /// fetched. `None` fetches every input of every lease. It must be on the scratch
    /// directory's volume (else leases get copies, still no fetch) and outside it: the
    /// sandbox lets an action write only its own lease directory, which keeps the
    /// entries out of its reach.
    pub cache: Option<Arc<Cache>>,
    /// The node's shared input trees (`kbf_daemon::trees`, delivered by cloning each
    /// whole into the lease, [`Delivery::Clone`]); needs `cache`, which builds the
    /// trees and writes the rest of the inputs. The store must be outside the scratch
    /// directory, like the cache. `None` clones every input file one by one.
    pub trees: Option<Arc<Trees>>,
    /// Reads the sandbox's refusals from the system's log for an action that failed
    /// under the sandbox ([`crate::denials`]); a test replaces it.
    pub denial_log: LogReader,
}

impl NativeConfig {
    /// A configuration with a one hour default timeout, the default output limits,
    /// upload deadline ([`kbf_daemon::UPLOAD_DEADLINE`]) and memory policy, a 250 ms poll, a 5 s kill wait, [`Listing::DEFAULT`], this node's isolation and user
    /// folders, leftovers swept after [`LEFTOVER_AGE`], no Xcode (see [`Self::xcodes`]),
    /// the system `xcrun`, no lease-user pool and no input cache.
    #[must_use]
    pub fn new(scratch: PathBuf) -> Self {
        Self {
            scratch,
            default_timeout: Duration::from_secs(3600),
            outputs: OutputLimits::DEFAULT,
            upload_deadline: kbf_daemon::UPLOAD_DEADLINE,
            memory: MemoryPolicy::DEFAULT,
            poll: Duration::from_millis(250),
            kill_wait: Duration::from_secs(5),
            simulator_listing: Listing::DEFAULT,
            isolation: Isolation::detect(),
            xcodes: BTreeMap::new(),
            user_folders: UserFolders::detect(),
            leftover_age: LEFTOVER_AGE,
            xcrun: PathBuf::from(crate::xcode::XCRUN),
            pool: None,
            cache: None,
            trees: None,
            denial_log: crate::denials::system_log,
        }
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
}
