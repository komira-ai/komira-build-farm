//! Swap a staged release in on trial and keep it only if it is confirmed: `activate`,
//! `confirm`, `revert`, `boot_check`, `expire` and `reconcile`, on the layout of
//! [`crate::layout`].
//!
//! The release was installed by [`crate::release::stage`] (or `install`); this module
//! moves the links. A release is never changed once installed, so the process running
//! the old one keeps its file: only `current` and `previous` move.
//!
//! The order of writes is the crash story. Every verb writes the whole target to
//! `state.json` first (replaced by a rename, then the directory synced) and only then
//! moves the links to match it, `previous` before `current`, each by an atomic rename.
//! So the state is always either the old target or the new, never a mix, and the links
//! can disagree with it only by an interrupted verb. [`reconcile`] makes them agree: the
//! state is the truth. A swap that was written is carried out, a revert that was written
//! is carried out, and the release `previous` names is intact either way.
//!
//! - [`activate`]: an order names a staged release. The state gets `current` = the new
//!   release, `previous` = the one that ran, `staged` cleared, and a `pending_confirm` naming the order, then the links follow. The caller
//!   restarts the service afterwards. A repeat of the same order, or an order for the
//!   release already current, changes nothing.
//! - [`confirm`]: the new release came up and says so; the swap is kept. It must name
//!   the order that made the swap.
//! - [`revert`]: `current` goes back to the release that ran before; the state is
//!   written first. If that release is gone, nothing is changed and the swap stays
//!   pending, for the operator to see.
//! - [`boot_check`] runs before every start of the service. The window to confirm starts
//!   at the first start after the swap, not at the swap: the frontend's own update can
//!   wait hours between the two. With a start budget (Linux), a release that was
//!   started that many times without a confirm is reverted before it starts again,
//!   which catches a crash loop in seconds, with no timer.
//! - [`expire`]: with a deadline (the window has started), a swap not confirmed in time
//!   is reverted. On macOS nothing runs before a start, so [`Arm::FromActivation`] starts
//!   the window at the swap and this is the only trigger.
//!
//! The window is bounded by the host ([`WINDOW_MIN_SECS`], [`WINDOW_MAX_SECS`]), not by
//! the order: a compromised frontend cannot ask for a year.

use std::io::Write;

use kbf_host::exec::Exec;
use kbf_host::plan::{Mode, Planner, RunAs, execute};

use crate::CommitId;
use crate::layout::{self, Layout, path};
use crate::release::{ReleaseError, has_binary};
use crate::state::{Locked, PendingConfirm, State};

/// The shortest confirm window a host accepts.
pub const WINDOW_MIN_SECS: u64 = 120;
/// The longest.
pub const WINDOW_MAX_SECS: u64 = 1800;

/// An order to swap a staged release in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub commit: CommitId,
    /// Names the order: a repeat is a no-op and the confirm must carry it.
    pub order_id: String,
}

/// When the window to confirm starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arm {
    /// At the swap, for `window_secs` (macOS: no start hook, so a timer is the only
    /// trigger).
    FromActivation { window_secs: u64 },
    /// At the first start of the new release ([`boot_check`]), after which `starts`
    /// unconfirmed starts are allowed in all (Linux).
    FromFirstStart { starts: u32 },
}

/// What [`activate`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Activated {
    /// The links and the state moved to the ordered release, which is on trial.
    Swapped,
    /// This order is already pending; the links were made to match the state.
    Repeat,
    /// The ordered release is already current and confirmed; nothing is pending.
    AlreadyCurrent,
}

/// What [`confirm`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmed {
    Kept,
    /// The swap was confirmed before; a repeat.
    AlreadyKept,
}

/// What [`revert`] and a reverting [`boot_check`] or [`expire`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reverted {
    /// The release that was current and is not now.
    pub from: Option<CommitId>,
    /// The release that is current now.
    pub to: CommitId,
}

/// What [`boot_check`] decided about the start that is about to happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Boot {
    /// Nothing is on trial; start.
    Nothing,
    /// A release is on trial; start it. `starts_left` is what remains after this one.
    Start { starts_left: Option<u32> },
    /// The release on trial was reverted; start the one that is current now.
    Reverted(Reverted),
}

/// What [`expire`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expiry {
    Nothing,
    /// A swap is pending and its window has not started.
    NotStarted,
    Waiting {
        secs_left: u64,
    },
    Reverted(Reverted),
}

fn window(secs: u64) -> Result<u64, ReleaseError> {
    if (WINDOW_MIN_SECS..=WINDOW_MAX_SECS).contains(&secs) {
        Ok(secs)
    } else {
        Err(ReleaseError::Window(secs))
    }
}

/// The lock and the state, which may be missing (an empty host).
fn open(host: &Layout) -> Result<(Locked, State), ReleaseError> {
    let lock = Locked::acquire(host)?;
    let state = lock.read()?.unwrap_or_default();
    Ok((lock, state))
}

/// Make `previous` and `current` point where `state` says, `previous` first. Whether a
/// link moved; a state with no current release has nothing to point at.
fn converge<E: Exec>(
    exec: &mut E,
    state: &State,
    out: &mut dyn Write,
) -> Result<bool, ReleaseError> {
    let Some(current) = &state.current else {
        return Ok(false);
    };
    let mut planner = Planner::new(&*exec);
    let mut moved = false;
    match &state.previous {
        Some(c) => moved |= planner.symlink(path(layout::PREVIOUS), layout::release_link(c))?,
        None => {
            planner.absent(path(layout::PREVIOUS))?;
        }
    }
    moved |= planner.symlink(path(layout::CURRENT), layout::release_link(current))?;
    let plan = planner.finish();
    execute(&plan, RunAs::Root, Mode::Apply, exec, &mut &mut *out)?;
    Ok(moved)
}

/// Swap the ordered release in on trial (see the module docs).
pub fn activate<E: Exec>(
    exec: &mut E,
    host: &Layout,
    order: &Order,
    arm: Arm,
    now: u64,
    out: &mut dyn Write,
) -> Result<Activated, ReleaseError> {
    let (deadline, starts_left) = match arm {
        Arm::FromActivation { window_secs } => (Some(now + window(window_secs)?), None),
        Arm::FromFirstStart { starts } => (None, Some(starts)),
    };
    let (lock, state) = open(host)?;
    if let Some(p) = &state.pending_confirm {
        if p.order_id.as_deref() != Some(order.order_id.as_str()) {
            return Err(ReleaseError::Pending(p.commit.clone()));
        }
        converge(exec, &state, out)?;
        return Ok(Activated::Repeat);
    }
    if state.current.as_ref() == Some(&order.commit) {
        converge(exec, &state, out)?;
        return Ok(Activated::AlreadyCurrent);
    }
    if !has_binary(exec, &order.commit) {
        return Err(ReleaseError::NotInstalled(order.commit.clone()));
    }
    let next = State {
        current: Some(order.commit.clone()),
        previous: state.current.clone(),
        staged: state.staged.clone().filter(|c| *c != order.commit),
        pending_confirm: Some(PendingConfirm {
            commit: order.commit.clone(),
            previous: state.current.clone(),
            deadline,
            order_id: Some(order.order_id.clone()),
            starts_left,
        }),
        order_floor: state.order_floor,
    };
    lock.write(&next)?;
    converge(exec, &next, out)?;
    Ok(Activated::Swapped)
}

/// Keep the swap that `order_id` made. `commit` is the release the order named, so that
/// a confirm after the swap was reverted is not taken for a repeat.
pub fn confirm(
    host: &Layout,
    order_id: &str,
    commit: &CommitId,
) -> Result<Confirmed, ReleaseError> {
    let (lock, state) = open(host)?;
    let Some(p) = &state.pending_confirm else {
        return if state.current.as_ref() == Some(commit) {
            Ok(Confirmed::AlreadyKept)
        } else {
            Err(ReleaseError::NotPending(commit.clone()))
        };
    };
    match p.order_id.as_deref() {
        Some(id) if id == order_id && p.commit == *commit => {}
        other => {
            return Err(ReleaseError::OtherOrder {
                commit: p.commit.clone(),
                pending: other.unwrap_or("(none)").to_owned(),
                got: order_id.to_owned(),
            });
        }
    }
    lock.write(&State {
        pending_confirm: None,
        ..state
    })?;
    Ok(Confirmed::Kept)
}

/// Put `current` back on the release that ran before the pending swap (with none
/// pending: on `previous`, which is then dropped), as [`Reverted`] says.
pub fn revert<E: Exec>(
    exec: &mut E,
    host: &Layout,
    out: &mut dyn Write,
) -> Result<Reverted, ReleaseError> {
    let (lock, state) = open(host)?;
    revert_in(exec, lock, state, out)
}

fn revert_in<E: Exec>(
    exec: &mut E,
    lock: Locked,
    state: State,
    out: &mut dyn Write,
) -> Result<Reverted, ReleaseError> {
    let back = match &state.pending_confirm {
        Some(p) => p.previous.clone(),
        None => state.previous.clone(),
    };
    let Some(to) = back else {
        return Err(ReleaseError::NoPrevious);
    };
    if !has_binary(exec, &to) {
        return Err(ReleaseError::PreviousGone(to));
    }
    let from = state.current.clone();
    let next = State {
        current: Some(to.clone()),
        previous: None,
        pending_confirm: None,
        ..state
    };
    lock.write(&next)?;
    converge(exec, &next, out)?;
    Ok(Reverted { from, to })
}

/// Run before every start of the service (see the module docs). `window_secs` is the
/// host's window, within [`WINDOW_MIN_SECS`] and [`WINDOW_MAX_SECS`].
pub fn boot_check<E: Exec>(
    exec: &mut E,
    host: &Layout,
    now: u64,
    window_secs: u64,
    out: &mut dyn Write,
) -> Result<Boot, ReleaseError> {
    let window_secs = window(window_secs)?;
    let (lock, state) = open(host)?;
    let Some(p) = state.pending_confirm.clone() else {
        return Ok(Boot::Nothing);
    };
    if p.starts_left == Some(0) || p.deadline.is_some_and(|d| now >= d) {
        return Ok(Boot::Reverted(revert_in(exec, lock, state, out)?));
    }
    let starts_left = p.starts_left.map(|n| n - 1);
    lock.write(&State {
        pending_confirm: Some(PendingConfirm {
            deadline: p.deadline.or(Some(now + window_secs)),
            starts_left,
            ..p
        }),
        ..state
    })?;
    Ok(Boot::Start { starts_left })
}

/// Revert a swap whose window has run out without a confirm.
pub fn expire<E: Exec>(
    exec: &mut E,
    host: &Layout,
    now: u64,
    out: &mut dyn Write,
) -> Result<Expiry, ReleaseError> {
    let (lock, state) = open(host)?;
    let Some(p) = &state.pending_confirm else {
        return Ok(Expiry::Nothing);
    };
    match p.deadline {
        None => Ok(Expiry::NotStarted),
        Some(d) if now < d => Ok(Expiry::Waiting { secs_left: d - now }),
        Some(_) => Ok(Expiry::Reverted(revert_in(exec, lock, state, out)?)),
    }
}

/// Make the links agree with the state, after a verb was interrupted. Whether a link
/// moved.
pub fn reconcile<E: Exec>(
    exec: &mut E,
    host: &Layout,
    out: &mut dyn Write,
) -> Result<bool, ReleaseError> {
    let (_lock, state) = open(host)?;
    converge(exec, &state, out)
}

#[cfg(test)]
#[path = "activate_tests.rs"]
mod tests;
