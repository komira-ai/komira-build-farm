//! Integration tests that run in hosted CI, including the workspace layering test
//! in `tests/layering.rs`, and the repository lints applied by `tests/repo_lints.rs`:
//! the workflow lint in `tests/workflows/` (a test-only module, so its YAML parser
//! stays a dev-dependency) and [`hygiene`] for public text.
//!
//! It also holds the M1 integration harness. `m1/run.sh` starts a cell (MinIO, one
//! `kbf-server` with in-memory metadata and the S3 store, one daemon) and builds the
//! sample projects in `m1/` remote-only with pinned Bazel and buck2, twice; the second
//! build must be all remote cache hits. With buck2 it also builds `//:pause`, an action
//! that sleeps, and drains the daemon's node through the operator API while that lease
//! is in flight (`m1/run.sh` says what it checks). The `kbf-cell` binary is its helper:
//! - `kbf-cell pki` writes the cell's throwaway certificates ([`pki`]);
//! - `kbf-cell daemon` runs the test-only daemon ([`daemon`], Linux only);
//! - `kbf-cell check` reads both builds' summaries and applies the exit rule
//!   ([`summary`]).
//!
//! And the M2 harness: `m2/run.sh` builds the sample project in `m2/` with pinned buck2
//! through one `kbf-server` to one `kbf-daemon --driver container` (every action in a
//! rootless Podman container of a pinned distroless image), twice, then checks an
//! own-limit memory kill's doubled rerun and a lease held in a container (`m2/run.sh`
//! says what it checks; docs/design/daemon.md, "End to end"). The image has no shell,
//! so every action runs the `kbf-m2-act` binary.

#[cfg(target_os = "linux")]
pub mod daemon;
pub mod hygiene;
pub mod pki;
pub mod summary;
