//! The `inputs phases:` line: where one container lease's prepare step spent its time,
//! one INFO line per lease whose prepare step succeeded, after the `inputs:` line (what
//! the files cost). `key=value` fields, space-separated, in a fixed order.
//!
//! Every `*_ms` field is whole milliseconds of wall time of one step, and the steps run
//! one after another, so they add up to about the prepare step (less the scratch
//! directories and the container spec, which are not timed). The daemon's
//! [`WritePhases`] (`walk_ms` to `copies`) splits the input root's write: with shared
//! trees `walk_ms` is the classifier's read plus the walk below the trees, without them
//! the walk alone; `trees_ms` and `trees_built` are 0 without trees, `links` is 0
//! without the cache.

use std::fmt;
use std::time::Instant;

use kbf_daemon::tree::WritePhases;

/// The fields of the `inputs phases:` line.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct InputPhases {
    /// Fetching the Action and the Command.
    pub action_ms: u64,
    /// Checking the action's image (`check_image`, `Oci::check_pinned`): one `stat` of
    /// its unpacked directory, except for the first lease of a digest, which pays the
    /// fetch and the unpack here.
    pub image_ms: u64,
    /// Writing the input root.
    pub write: WritePhases,
    /// Trees the write built (the rest were there, or another lease built them).
    pub trees_built: u64,
    /// `refuse_hidden_working_directory` and `prepare_outputs`.
    pub prep_ms: u64,
    /// Giving the overlay to the container's user (`give_overlay`, `Oci::give`): one
    /// helper container running `__chown-dirs`.
    pub chown_ms: u64,
    /// The cgroup: its OOM count, creating it, and its keeper.
    pub cgroup_ms: u64,
    /// The NUMA node the lease is placed on (`--lease-placement=numa-node`); `None`, and
    /// no field on the line, when leases are not placed.
    pub numa_node: Option<Spot>,
}

/// Where a placed lease runs (`numa_node=` on the line).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// One NUMA node.
    Node(u32),
    /// Every CPU and memory node the daemon may use.
    All,
}

impl fmt::Display for Spot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Node(node) => write!(f, "{node}"),
            Self::All => f.write_str("all"),
        }
    }
}

impl fmt::Display for InputPhases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = &self.write;
        let numa = (self.numa_node).map_or_else(String::new, |spot| format!(" numa_node={spot}"));
        write!(
            f,
            "action_ms={} image_ms={} walk_ms={} dir_reads={} dirs_made={} trees_ms={} \
             trees_built={} link_ms={} links={} copies={} prep_ms={} chown_ms={} cgroup_ms={}{numa}",
            self.action_ms,
            self.image_ms,
            w.walk_ms,
            w.dir_reads,
            w.dirs_made,
            w.trees_ms,
            self.trees_built,
            w.link_ms,
            w.links,
            w.copies,
            self.prep_ms,
            self.chown_ms,
            self.cgroup_ms,
        )
    }
}

/// Whole milliseconds since `started`.
pub(crate) fn millis(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[path = "input_phases_tests.rs"]
mod tests;
