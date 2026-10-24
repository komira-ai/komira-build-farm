//! The clean of a slot between two leases (docs/design/macos-end-state.md, step 4): the
//! order of its steps and the rule for a step that fails. What each step does to the Mac
//! is an [`Os`]'s ([`super::macos`]); the rules here are the same on every platform, and
//! tested on every one with a fake.
//!
//! [`clean`] runs every [`Step`] of [`STEPS`] once, in that order, then looks for what is
//! left ([`Os::left`]) twice, [`Os::pause`] apart, so a job that launchd or cron started
//! again after the kill is seen. There is **one pass**: no retry, no third attempt, no
//! making the user again. A pass that fails leaves the user **dirty** ([`Outcome::Dirty`]):
//! the slot is not offered to a lease, the node reports one slot fewer, nothing is
//! written to disk about it, and it is cleaned again before it is offered
//! ([`super::book`]). The old retry hid two failures that were order defects; the order
//! below is their fix, and a failure that still happens is shown, not retried.
//!
//! The order, C1 to C11 ([`Step::code`]):
//!
//! 1. [`Step::Bootout`]: end the slot's launchd domains, whole, and wait until the domain
//!    lists nothing of the lease's.
//! 2. [`Step::Schedules`]: the uid's crontab and `at` jobs, before the kill: no job
//!    starts after it.
//! 3. [`Step::Kill`]: every process of the uid.
//! 4. [`Step::Images`]: the disk images and volumes the uid mounted.
//! 5. [`Step::Ipc`]: the System V IPC objects.
//! 6. [`Step::Flags`]: the user flags, ACLs and modes that would stop the removal,
//!    **roots first**: the home, then what is below it, as root; the `/var/folders`
//!    entry the same way but **as the slot's uid** (root cannot do it there).
//! 7. [`Step::Contents`]: the home emptied; the entry's `0`, `C`, `T` and `X` emptied (as
//!    the slot's uid) and kept, anything else in the entry removed.
//! 8. [`Step::Shared`]: the uid's files in the shared places.
//! 9. [`Step::Crashes`]: the crash reports about the uid.
//! 10. [`Step::Restore`]: the home and the entry's folders back to the owner, mode and
//!     flags they are given.
//! 11. [`Step::Look`]: a second look at the uid's processes, a pause after the first.
//!
//! Two rules keep the steps from racing the slot's processes. Nothing after
//! [`Step::Kill`] runs while a process of the uid lives: a failed kill ends the pass (the
//! sweeps walk only what no process of the uid can change). And a user is never deleted:
//! a user deleted under its own processes would leave them running as an orphan uid, and
//! the next user of the uid would share them.

use std::fmt;

/// One step of the clean, in the order [`clean`] runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// C1. The slot's launchd domains `user/<uid>` and `gui/<uid>` booted out whole (no
    /// job by its label), then waited for: launchd starts no `KeepAlive` or on-demand job
    /// of the uid again, and the domain lists only launchd's own.
    Bootout,
    /// C2. The uid's crontab and `at` jobs, before the kill: no job starts after it.
    Schedules,
    /// C3. SIGTERM to every process of the uid and a grace for them to end
    /// ([`crate::grace`]), then SIGKILL to every process of the uid, by effective or
    /// real uid, until two scans 200 ms apart find none (at most 100 scans, within a
    /// deadline).
    Kill,
    /// C4. The disk images the uid attached, detached, and every mount under its folders,
    /// unmounted.
    Images,
    /// C5. The System V IPC objects the uid owns or made, removed.
    Ipc,
    /// C6. The flags `uchg`, `uappnd`, `nodump`, `opaque` and `hidden` cleared, ACLs
    /// removed and the owner's mode made `u+rwx`, on the roots first (the home and the
    /// entry), then below them. A locked root stops the removal below it, so this is
    /// before [`Step::Contents`].
    Flags,
    /// C7. The home emptied; the `/var/folders` entry's `0`, `C`, `T` and `X` emptied and
    /// kept, and anything else in the entry removed whole.
    Contents,
    /// C8. The uid's files in the shared places (`/private/tmp`, `/private/var/tmp`, the
    /// `Shared` folder under `/Users`, `/Library/Caches`) and launchd's per-uid files.
    Shared,
    /// C9. The crash reports about the uid, in the crash folders and `/cores`.
    Crashes,
    /// C10. The home back to the uid:gid, mode 0700, no flags; the entry's roots to their
    /// recorded owner and flags, with `hidden` cleared.
    Restore,
    /// C11. A second look at the uid's processes, 200 ms after the first: a respawn is
    /// seen.
    Look,
}

/// Every step, in the order [`clean`] runs them.
pub const STEPS: [Step; 11] = [
    Step::Bootout,
    Step::Schedules,
    Step::Kill,
    Step::Images,
    Step::Ipc,
    Step::Flags,
    Step::Contents,
    Step::Shared,
    Step::Crashes,
    Step::Restore,
    Step::Look,
];

impl Step {
    /// The step's number in the order: `C1` to `C11`.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Bootout => "C1",
            Self::Schedules => "C2",
            Self::Kill => "C3",
            Self::Images => "C4",
            Self::Ipc => "C5",
            Self::Flags => "C6",
            Self::Contents => "C7",
            Self::Shared => "C8",
            Self::Crashes => "C9",
            Self::Restore => "C10",
            Self::Look => "C11",
        }
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Bootout => "bootout",
            Self::Schedules => "schedules",
            Self::Kill => "kill",
            Self::Images => "images",
            Self::Ipc => "ipc",
            Self::Flags => "flags",
            Self::Contents => "contents",
            Self::Shared => "shared",
            Self::Crashes => "crashes",
            Self::Restore => "restore",
            Self::Look => "look",
        };
        f.write_str(name)
    }
}

/// Something a clean slot does not have, with the step that should have removed it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Left {
    pub step: Step,
    pub what: String,
}

/// The Mac's part of one slot's clean.
pub trait Os {
    /// Runs `step` for the slot's user.
    ///
    /// # Errors
    /// What the step could not do.
    fn run(&self, step: Step) -> Result<(), String>;

    /// What is left that a clean slot does not have; empty when it is clean.
    fn left(&self) -> Vec<Left>;

    /// The wait between the check's two looks.
    fn pause(&self);
}

/// How the check after a clean with a [`Skipped`] step answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// The inner [`Os::left`] answers: what the missing step left is found, so the clean
    /// fails and the slot ends dirty, never clean.
    Honest,
    /// [`Os::left`] answers nothing is left, whatever the inner one finds (all of it,
    /// not only the missing step's: a missing step can leave work for another's look,
    /// such as a job launchd starts again after the kill). What the missing step should
    /// have removed reaches the next lease: the residue rows of a freshness test stay.
    Blind,
}

/// A test-only option: one [`Step`] left out of the clean, and the check honest or blind.
/// For the tests that show each step is needed (`tests/slot_steps.rs` on every platform,
/// `tests/slot_clean.rs` on a Mac): with a step left out, the rows it removes stay.
/// Nothing in the daemon builds one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Skipped {
    pub skip: Step,
    pub check: Check,
}

/// An [`Os`] that is `inner` except as its [`Skipped`] option says: [`Os::run`] of the missing
/// step does nothing and succeeds, [`Os::left`] is empty when the check is blind. The
/// order and the pause are `inner`'s.
pub struct WithSkipped<'a> {
    inner: &'a dyn Os,
    skipped: Skipped,
}

impl<'a> WithSkipped<'a> {
    #[must_use]
    pub fn new(inner: &'a dyn Os, skipped: Skipped) -> Self {
        Self { inner, skipped }
    }
}

impl Os for WithSkipped<'_> {
    fn run(&self, step: Step) -> Result<(), String> {
        if step == self.skipped.skip {
            return Ok(());
        }
        self.inner.run(step)
    }

    fn left(&self) -> Vec<Left> {
        let left = self.inner.left();
        match self.skipped.check {
            Check::Honest => left,
            Check::Blind => Vec::new(),
        }
    }

    fn pause(&self) {
        self.inner.pause();
    }
}

/// How a clean ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The slot is clean.
    Clean,
    /// A step failed or the check found something left: the user is dirty, and the slot
    /// is not offered until a clean passes. `why` says what failed.
    Dirty { why: String },
}

/// Cleans one slot through `os`, once: see the module documentation for the rules.
pub fn clean(os: &dyn Os) -> Outcome {
    let mut failed = Vec::new();
    for step in STEPS {
        if let Err(why) = os.run(step) {
            failed.push(format!("{step}: {why}"));
            if step == Step::Kill {
                return Outcome::Dirty {
                    why: failed.join("; "),
                };
            }
        }
    }
    let mut left = os.left();
    if left.is_empty() {
        os.pause();
        left = os.left();
    }
    if failed.is_empty() && left.is_empty() {
        return Outcome::Clean;
    }
    failed.extend(
        left.into_iter()
            .map(|l| format!("left ({}): {}", l.step, l.what)),
    );
    Outcome::Dirty {
        why: failed.join("; "),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::{BTreeSet, VecDeque};

    use super::*;

    /// What the fake was asked to do, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Run(Step),
        Left,
        Pause,
    }

    /// An [`Os`] that records its calls: each step fails as many times as `fails` says
    /// and each look answers the next entry of `looks` (then nothing is left).
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<Call>>,
        fails: RefCell<BTreeSet<Step>>,
        looks: RefCell<VecDeque<Vec<Left>>>,
    }

    impl Fake {
        fn failing(step: Step) -> Self {
            let fake = Self::default();
            fake.fails.borrow_mut().insert(step);
            fake
        }

        fn looking(looks: Vec<Vec<Left>>) -> Self {
            let fake = Self::default();
            *fake.looks.borrow_mut() = looks.into();
            fake
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.borrow().clone()
        }
    }

    impl Os for Fake {
        fn run(&self, step: Step) -> Result<(), String> {
            self.calls.borrow_mut().push(Call::Run(step));
            // Fails once.
            if self.fails.borrow_mut().remove(&step) {
                Err(format!("{step} failed"))
            } else {
                Ok(())
            }
        }

        fn left(&self) -> Vec<Left> {
            self.calls.borrow_mut().push(Call::Left);
            self.looks.borrow_mut().pop_front().unwrap_or_default()
        }

        fn pause(&self) {
            self.calls.borrow_mut().push(Call::Pause);
        }
    }

    /// The calls of one pass that runs every step and looks twice.
    fn whole_pass() -> Vec<Call> {
        let mut calls: Vec<Call> = STEPS.iter().copied().map(Call::Run).collect();
        calls.extend([Call::Left, Call::Pause, Call::Left]);
        calls
    }

    fn left(step: Step, what: &str) -> Left {
        Left {
            step,
            what: what.to_owned(),
        }
    }

    fn dirty(why: &str) -> Outcome {
        Outcome::Dirty {
            why: why.to_owned(),
        }
    }

    /// Catches a step missing, added or run out of the design's order C1 to C11 (a
    /// kill before the bootout lets launchd start the uid's agents again; a removal
    /// before the kill races the uid's processes; a removal before the root flags is
    /// the defect of a locked home), a check that looks only once (a job started again
    /// after the kill is missed), a retry or a second pass on the normal path, and step
    /// codes that are not the numbers the design gives: a clean slot runs each step
    /// once in exactly this order, looks twice a pause apart, and is clean.
    #[test]
    fn a_clean_slot_runs_c1_to_c11_in_order_once_and_looks_twice() {
        assert_eq!(
            STEPS.map(|s| s.to_string()),
            [
                "bootout",
                "schedules",
                "kill",
                "images",
                "ipc",
                "flags",
                "contents",
                "shared",
                "crashes",
                "restore",
                "look"
            ]
        );
        assert_eq!(
            STEPS.map(Step::code),
            [
                "C1", "C2", "C3", "C4", "C5", "C6", "C7", "C8", "C9", "C10", "C11"
            ]
        );
        let fake = Fake::default();
        assert_eq!(clean(&fake), Outcome::Clean);
        assert_eq!(fake.calls(), whole_pass());
    }

    /// A model of the home that was locked by its user: the root carries `uchg` until
    /// [`Step::Flags`] clears it, and [`Step::Contents`] cannot remove an entry of a
    /// locked root.
    #[derive(Default)]
    struct Locked {
        locked: Cell<bool>,
    }

    impl Os for Locked {
        fn run(&self, step: Step) -> Result<(), String> {
            match step {
                Step::Flags => self.locked.set(false),
                Step::Contents if self.locked.get() => {
                    return Err("home: Operation not permitted".to_owned());
                }
                _ => {}
            }
            Ok(())
        }

        fn left(&self) -> Vec<Left> {
            Vec::new()
        }

        fn pause(&self) {}
    }

    /// Catches the defect of the home root with `chflags uchg` set by its user: the
    /// removal that runs before the flags are cleared cannot empty a locked directory,
    /// so the clean fails on its single pass. With the root flags first, the one pass
    /// is clean.
    #[test]
    fn a_home_locked_by_its_user_is_cleaned_on_the_single_pass() {
        let os = Locked::default();
        os.locked.set(true);
        // The model refuses a removal while the root is locked.
        assert_eq!(
            os.run(Step::Contents),
            Err("home: Operation not permitted".to_owned())
        );
        assert_eq!(clean(&os), Outcome::Clean);
        assert!(
            STEPS.iter().position(|s| *s == Step::Flags)
                < STEPS.iter().position(|s| *s == Step::Contents)
        );
    }

    /// Catches a check whose findings are ignored (what a step left would reach the next
    /// lease), a second look ignored (what came back after the pause), and a retry after
    /// either: a slot with something left at either look is dirty after one pass and
    /// names what it found.
    #[test]
    fn what_either_look_finds_leaves_the_user_dirty_after_one_pass() {
        let fake = Fake::looking(vec![vec![left(Step::Contents, "a file")]]);
        assert_eq!(clean(&fake), dirty("left (contents): a file"));
        let mut want: Vec<Call> = STEPS.iter().copied().map(Call::Run).collect();
        want.push(Call::Left);
        assert_eq!(fake.calls(), want);

        let fake = Fake::looking(vec![vec![], vec![left(Step::Bootout, "an agent")]]);
        assert_eq!(clean(&fake), dirty("left (bootout): an agent"));
        assert_eq!(fake.calls(), whole_pass());
    }

    /// Catches a failed step that does not fail the clean, a step failure that stops the steps after it (one stuck image would
    /// leave the home unemptied), a retry of the pass, and a recreate of the user: a
    /// step that fails leaves the user dirty with its reason, after one pass with every
    /// step run, and the fake is never asked to try again.
    #[test]
    fn a_failed_step_leaves_the_user_dirty_after_one_pass() {
        let fake = Fake::failing(Step::Images);
        assert_eq!(clean(&fake), dirty("images: images failed"));
        assert_eq!(fake.calls(), whole_pass());
        let fake = Fake::failing(Step::Crashes);
        fake.fails.borrow_mut().insert(Step::Contents);
        assert_eq!(
            clean(&fake),
            dirty("contents: contents failed; crashes: crashes failed")
        );
        assert_eq!(fake.calls(), whole_pass());
    }

    /// Catches steps that run after a failed kill (sweeps racing live processes), and a
    /// slot given back with survivors: a kill that fails ends the pass at once, the
    /// slot is dirty, and no step after C3 ran. Survivors the check finds after a pass
    /// are dirty too, and the pass is not repeated.
    #[test]
    fn survivors_leave_the_user_dirty_and_a_failed_kill_ends_the_pass() {
        let fake = Fake::failing(Step::Kill);
        assert_eq!(clean(&fake), dirty("kill: kill failed"));
        let upto = [Step::Bootout, Step::Schedules, Step::Kill].map(Call::Run);
        assert_eq!(fake.calls(), upto);

        let survivor = || vec![left(Step::Kill, "pid 7"), left(Step::Contents, "a file")];
        let fake = Fake::looking(vec![survivor()]);
        assert_eq!(
            clean(&fake),
            dirty("left (kill): pid 7; left (contents): a file")
        );
        assert_eq!(fake.calls().len(), STEPS.len() + 1);
    }

    /// Catches a skipped-step option that skips more or less than the one step it names
    /// (the necessity tests would then blame the wrong step), or that reorders, drops or
    /// doubles the rest: for every step, the inner [`Os`] is asked the other ten in [`STEPS`]' order
    /// and never the missing one, and the clean is whole.
    #[test]
    fn a_skipped_step_is_the_only_one_not_run() {
        for skip in STEPS {
            let fake = Fake::default();
            let with_skip = WithSkipped::new(
                &fake,
                Skipped {
                    skip,
                    check: Check::Blind,
                },
            );
            assert_eq!(clean(&with_skip), Outcome::Clean, "skipping {skip}");
            let mut want = whole_pass();
            want.retain(|c| *c != Call::Run(skip));
            assert_eq!(fake.calls(), want, "skipping {skip}");
        }
    }

    /// Catches a blind check that lets the inner look through, and an honest one that
    /// blinds: what the inner look finds is hidden from the clean when blind (clean
    /// although something is left, which is what lets a test see what a missing step
    /// leaves) and seen when honest (the slot is dirty).
    #[test]
    fn a_blind_check_hides_what_the_inner_look_finds_and_an_honest_one_does_not() {
        let looks = || vec![vec![left(Step::Contents, "a file")]; 2];
        let blind = Fake::looking(looks());
        let skipped = Skipped {
            skip: Step::Contents,
            check: Check::Blind,
        };
        assert_eq!(clean(&WithSkipped::new(&blind, skipped)), Outcome::Clean);
        let mut want = whole_pass();
        want.retain(|c| *c != Call::Run(Step::Contents));
        assert_eq!(blind.calls(), want);

        let honest = Fake::looking(looks());
        let skipped = Skipped {
            check: Check::Honest,
            ..skipped
        };
        assert_eq!(
            clean(&WithSkipped::new(&honest, skipped)),
            dirty("left (contents): a file")
        );
    }
}
