//! The native driver for `kbf-daemon`: runs each `action` lease as plain processes on
//! the node, for Macs (where there are no containers), behind the daemon's `Runtime`
//! trait.
//!
//! What one lease gets:
//! - a fresh lease directory under the configured scratch root, the input root written
//!   into it, the working directory and each output's parent made (REAPI);
//! - its own home, temporary and cache directories inside the lease directory, named by
//!   `HOME`, `TMPDIR`, `XDG_CACHE_HOME` and `CLANG_MODULE_CACHE_PATH` (`home`); so a
//!   tool's `~` and module caches go with the lease;
//! - its `arguments` run with those variables and the Command's environment (which wins
//!   where both name a variable) and nothing else, stdin from `/dev/null`, stdout and
//!   stderr captured to files, as the leader of a new process group;
//! - on macOS, a `sandbox-exec` sandbox around every action ([`network`]): no file
//!   written outside the lease directory and `/dev`, no user preference written through
//!   `cfprefsd` (`defaults write`), no work handed to launchd
//!   (`open`, `launchctl submit`/`load`/`bootstrap`), and no network unless the
//!   action's `network` platform property allows it; elsewhere nothing is enforced,
//!   and the node reports which in its `network_isolation` capability;
//! - the Xcode its `xcode` platform property names, as `DEVELOPER_DIR` ([`xcode`]: the
//!   driver advertises every ready Xcode build, reports every installed Xcode with why
//!   one is not ready and how to fix it, and asks again every few minutes,
//!   [`xcode_watch`]);
//! - a memory watch: every poll, the physical footprint of all the action's processes
//!   together ([`procs`]); past the lease's limit, the whole tree is killed and the
//!   lease ends RESOURCE_EXHAUSTED ([`kbf_daemon::RuntimeError::OutOfMemory`]);
//! - a wall-clock timeout, and the daemon's kill, each ending the whole process tree;
//! - on every path, also a normal exit, every process of the action ended before the
//!   outputs are read: SIGKILL to the group and to each process the tracker knows,
//!   repeated until a snapshot shows none alive; any that survive fail the lease;
//! - outputs, stdout and stderr read with `kbf-outputs`: no symlink followed at any
//!   level; depth, entry, byte and stdio limits; files of more than a chunk streamed
//!   to the CAS ([`kbf_daemon::Cas::put_chunks`]); names that are not UTF-8 refused;
//! - the lease directory removed afterwards (`kbf_outputs::remove_tree`: iterative,
//!   by descriptor, giving back permissions the action took away and, on macOS,
//!   clearing the immutable and append-only flags and the ACLs it set); a lease directory
//!   that stays fails the lease, so a dirty node is loud. A run the daemon drops is
//!   cleaned by `Drop`.
//!
//! The container driver's own output walk is public too, but does not build on macOS;
//! moving it onto `kbf-outputs` is a follow-up.
//!
//! Before an action's program runs, its run record (the leader's pid, start time and
//! boot) is written under `runs/` in the scratch root (`record`): the child waits
//! between fork and exec until it is. At start the runtime kills every action a
//! previous daemon recorded and left running, the daemon having been killed (`sweep`;
//! processes of the daemon's user that survive SIGKILL stop the start; anything in
//! `runs/` that is not a record of the daemon's is moved aside into `quarantine/`, and
//! another user's processes are left alone), then removes every lease directory it left;
//! one that cannot be removed is moved aside into `quarantine/` under the scratch root
//! and logged, and the daemon starts anyway (an action decides what its directory
//! holds, so refusing to start would let one build step take the node out of the farm).
//!
//! Known gaps, all of them closed only by running each lease as its own user:
//! - **Processes that leave the tree.** A process that calls `setsid` and is orphaned
//!   between two polls is not seen ([`procs`]), so it outlives the lease. Work handed
//!   to launchd would run outside the tree and the sandbox: every action is
//!   sandboxed, so launchd refuses it a job and the profile denies it `open`
//!   ([`network::BASE_PROFILE`]). Not covered: other mach and XPC services, Apple
//!   Events among them (`osascript` asking Terminal or Finder to run something), and
//!   anything else the daemon's user can schedule (a `LaunchAgents` plist run at its
//!   next login, `at`, `cron`, a loopback service).
//! - **The daemon's own files.** Actions run as the daemon's user, so they can read
//!   what it can, the node's TLS private key (`--key`) included. On macOS the
//!   profile keeps their file writes inside their own lease directory: the ways
//!   `tests/sandbox.rs` tries on the macOS runner (create, append, unlink, rename,
//!   chmod, mkdir, extended attributes, `/tmp`, a hard link to a file outside the lease
//!   or in another lease, a preference write) are all refused, so none of them changes
//!   other leases' directories, the scratch root, `quarantine/` or the daemon's
//!   configuration. A service that writes on an action's behalf by another route
//!   than those is not covered (the mach and XPC gap above). On Linux (no sandbox)
//!   they can, and an action that locks the
//!   scratch root or `quarantine/` (`chmod 555`) makes every later lease on the node
//!   fail until an operator unlocks it.
//! - **The signal race.** [`procs::kill_all`] signals pids from a snapshot; one
//!   recycled in between is a process of the daemon's user killed by mistake.
//! - **The run records.** The start-up sweep trusts `runs/` because only the daemon
//!   writes it: on macOS the sandbox keeps actions out of it. On Linux an action, which
//!   runs as the daemon's user, can write a well-formed record there (pids and start
//!   times are readable in `/proc`), and the next start SIGKILLs every process of that
//!   user in the group it names or below it. That holds for a group the daemon may not
//!   signal too: another user's group is left alone, but the walk goes on below it and
//!   kills the daemon's user's processes there. Only a record naming the daemon's own
//!   group or its parent's is dropped without its group being walked. What is not a
//!   regular file of the daemon's user that only it may write is set aside unread, and
//!   neither another user's processes nor those the daemon never signals
//!   ([`procs::Guards`]) are counted as survivors, so a forged entry cannot stop or hang
//!   a start.
//! - **Network "off" is not airtight**: Unix sockets stay open, the system resolver's
//!   among them, so DNS lookups still leave the node and can carry data ([`network`]).
//!
//! Not yet: a per-lease user (which would make "every process of the action" exact,
//! keep actions out of each other's files and the daemon's key, and close the gaps
//! above), CPU limits, resource usage in the result, and network isolation on Linux.

mod cas;
mod config;
mod home;
pub mod network;
pub mod procs;
mod record;
mod runtime;
mod sweep;
pub mod xcode;
pub mod xcode_watch;

pub use config::{MemoryPolicy, NativeConfig};
pub use runtime::{DRIVER, KIND, NativeRuntime};
