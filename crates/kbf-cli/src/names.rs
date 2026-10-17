//! The one table of modes: their names and what each runs.
//!
//! Every place in this crate that spells a mode reads it from here: the clap commands,
//! the dispatch, the help and the tests. The names follow the arch-v2 design; renaming
//! a mode is changing its constant.

/// The frontend (the REAPI front end).
pub const FRONTEND: &str = "frontend";
/// The storage node (the server half).
pub const STORAGE: &str = "storage";
/// The build node.
pub const DAEMON: &str = "daemon";
/// Storage and build node in one process.
pub const STORAGE_AND_DAEMON: &str = "storage-and-daemon";
/// Frontend, storage and build node in one process.
pub const ALL_IN_ONE: &str = "all-in-one";

/// What a mode runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The worker daemon: `kbf_node::run`, the flags of `kbf-daemon`.
    Daemon,
    /// The server and the daemon in one process: `kbf_all_in_one::run`, the flags of
    /// `kbf-all-in-one`.
    AllInOne,
    /// The storage node: `kbf_server::storage_node::run`, `kbf-server`'s store, write and
    /// log flags and its own listener flags.
    Storage,
    /// The storage node and the daemon in one process:
    /// `kbf_all_in_one::storage_and_daemon::run`, the flags of `kbf storage` and of
    /// `kbf-daemon`.
    StorageAndDaemon,
    /// The frontend: `kbf_server::frontend::run`, `kbf-server`'s listener, scheduler and
    /// log flags and its own storage flags.
    Frontend,
}

/// A mode and what it runs.
#[derive(Clone, Copy, Debug)]
pub struct Mode {
    /// The word after `kbf`.
    pub name: &'static str,
    /// What it runs.
    pub entry: Entry,
}

/// Every mode, in help order: exactly five.
pub const MODES: [Mode; 5] = [
    Mode {
        name: FRONTEND,
        entry: Entry::Frontend,
    },
    Mode {
        name: STORAGE,
        entry: Entry::Storage,
    },
    Mode {
        name: DAEMON,
        entry: Entry::Daemon,
    },
    Mode {
        name: STORAGE_AND_DAEMON,
        entry: Entry::StorageAndDaemon,
    },
    Mode {
        name: ALL_IN_ONE,
        entry: Entry::AllInOne,
    },
];
