//! `kbf-helper`: the root helper of the pull-based updates
//! (`docs/design/pull-updates.md`, section 7).
//!
//! It has no socket and no network. The unprivileged `kbf` leaves two kinds of file in the
//! inbox under `--update-dir`: an entry directory per offer (`<generation>-<commit>/`,
//! with `offer.json`, the tarballs and the attestation bundle) and, in `orders/`, one
//! file per order. Each pass the helper
//!
//! 1. finishes a restart it owes and reverts a swap whose confirm window ran out;
//! 2. stages the newest entry through [`kbf_installer::stage::stage_from_inbox`] (the
//!    attestation, hashes and binary checks all live there; the helper trusts nothing in
//!    the inbox);
//! 3. acts on orders, in id order, once each: `activate` swaps the staged release in on
//!    trial and restarts the one configured service, `confirm` keeps it, `revert` goes
//!    back;
//! 4. writes `status.json` (root's, world-readable) for the node to report.
//!
//! `kbf-helper boot-check` runs as the service's `ExecStartPre` on Linux: it counts the
//! starts of a release on trial and reverts one that keeps failing before it starts
//! again, which needs no timer. It also starts the confirm window at the first start.
//!
//! Nothing here takes a path, a command or a unit from the inbox: the unit and the
//! directories are the helper's own flags.

pub mod cycle;
pub mod memory;
pub mod order;
pub mod restart;
pub mod status;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod crash_tests;
#[cfg(test)]
mod tests;
