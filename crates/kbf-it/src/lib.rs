//! Integration tests that run in hosted CI, including the workspace layering test
//! in `tests/layering.rs`, and the repository lints applied by `tests/repo_lints.rs`:
//! the workflow lint in `tests/workflows/` (a test-only module, so its YAML parser
//! stays a dev-dependency) and [`hygiene`] for public text, and the check of the
//! example systemd units in `docs/deploy/` against the binaries' flag parsers
//! ([`deploy_units`], `tests/deploy_units.rs`).
//!
//! It also holds the M1 integration harness, the `kbf-cell` binary:
//! - `kbf-cell m1` ([`m1`], Linux only) starts a cell (MinIO, one `kbf-server` with
//!   in-memory metadata and the S3 store, one daemon) and builds the sample projects in
//!   `m1/` remote-only with pinned Bazel and buck2, twice; the second build must be all
//!   remote cache hits. With buck2 it also builds `//:pause`, an action that sleeps, and
//!   drains the daemon's node through the operator API while that lease is in flight
//!   ([`m1`] says what it checks);
//! - `kbf-cell pki` writes the cell's throwaway certificates ([`pki`]);
//! - `kbf-cell daemon` runs the test-only daemon ([`daemon`], Linux only);
//! - `kbf-cell check` reads both builds' summaries and applies the exit rule
//!   ([`summary`]).
//!
//! And the M2 harness, `kbf-cell m2` ([`m2`], Linux only): it builds the sample project
//! in `m2/` with pinned buck2 through one `kbf-server` to one `kbf-daemon --driver
//! container` (every action in a rootless `crun` container of a pinned distroless
//! image), twice, then checks the daemon's cgroup layout, an own-limit memory kill's
//! doubled rerun, a lease held in a container, and a daemon killed mid-lease ([`m2`]
//! says what it checks; docs/design/daemon.md, "End to end"). The image has no shell,
//! so every action runs the `kbf-m2-act` binary. Both cells share the private `cell`
//! module.

#[cfg(target_os = "linux")]
mod cell;
#[cfg(target_os = "linux")]
pub mod daemon;
pub mod deploy_units;
pub mod hygiene;
#[cfg(target_os = "linux")]
pub mod m1;
#[cfg(target_os = "linux")]
pub mod m2;
pub mod pki;
pub mod summary;

/// Why `kbf-cell m1` or `kbf-cell m2` did not pass.
#[cfg(target_os = "linux")]
pub use cell::CellError;
