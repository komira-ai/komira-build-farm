//! The clean of a slot between two leases (docs/design/macos-end-state.md, step 4): the
//! order of its steps, the check after them, the one retry, the recreate of the user,
//! and the quarantine. What each step does to the Mac is an [`Os`]'s
//! ([`super::macos`]); the rules here are the same on every platform, and tested on
//! every one with a fake.
//!
//! [`clean`] runs every [`Step`] in [`STEPS`]' order, then looks for what is left
//! ([`Os::left`]) twice, [`Os::pause`] apart, so a job that launchd or cron started
//! again after the kill is seen. A clean that fails is run once more. One that fails
//! again makes the user again ([`Os::recreate`]) and runs once more; if the recreate or
//! that clean fails too, the slot is quarantined.
//!
//! Two rules keep the steps from racing the slot's processes. Nothing after
//! [`Step::Kill`] runs while a process of the uid lives: a failed kill ends that
//! attempt (the sweeps walk only what no process of the uid can change). And a slot
//! whose processes survived is quarantined without a recreate: a user deleted under
//! its own processes would leave them running as an orphan uid, and the next user of
//! the uid would share them.

use std::fmt;

/// One step of the clean, in the order [`clean`] runs them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Step {
    /// `launchctl bootout user/<uid>`: launchd starts no `KeepAlive` or on-demand job
    /// of the uid again.
    Bootout,
    /// The uid's crontab and `at` jobs, before the kill: no job starts after it.
    Schedules,
    /// Every process of the uid, by effective or real uid, within a deadline.
    Kill,
    /// The disk images the uid attached, detached.
    Images,
    /// The System V IPC objects the uid owns or made, removed.
    Ipc,
    /// The home, back to an empty folder of the user's, mode 0700.
    Home,
    /// The uid's `/var/folders` entry: every folder in it (`0`, `C`, `T`) emptied.
    Folders,
    /// The uid's entries in the shared places (`/private/tmp`, `/private/var/tmp`,
    /// the `Shared` folder under `/Users`) and launchd's per-uid files.
    Shared,
}

/// Every step, in the order [`clean`] runs them.
pub const STEPS: [Step; 8] = [
    Step::Bootout,
    Step::Schedules,
    Step::Kill,
    Step::Images,
    Step::Ipc,
    Step::Home,
    Step::Folders,
    Step::Shared,
];

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Bootout => "bootout",
            Self::Schedules => "schedules",
            Self::Kill => "kill",
            Self::Images => "images",
            Self::Ipc => "ipc",
            Self::Home => "home",
            Self::Folders => "folders",
            Self::Shared => "shared",
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

    /// Deletes the slot's user and makes it again with the same name and uid, its home
    /// and its `/var/folders` entry new (the old ones moved where the uid cannot reach
    /// them). Never called while a process of the uid lives.
    ///
    /// # Errors
    /// What could not be done; the slot is then quarantined.
    fn recreate(&self) -> Result<(), String>;
}

/// How a clean ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The slot is clean (perhaps at the second attempt).
    Clean,
    /// The clean failed twice; the user was made again, and the clean after that
    /// passed. `why` is the failure that led to it.
    Recreated { why: String },
    /// The slot cannot be used again until the node is looked at: `why` says what
    /// failed last.
    Quarantined { why: String },
}

/// Why one attempt failed.
struct Failed {
    /// A process of the uid survived: no recreate.
    processes: bool,
    why: String,
}

/// Cleans one slot through `os`: see the module documentation for the rules.
pub fn clean(os: &dyn Os) -> Outcome {
    let Err(first) = attempt(os) else {
        return Outcome::Clean;
    };
    tracing::warn!(why = %first.why, "the clean of a slot failed; trying once more");
    let Err(again) = attempt(os) else {
        return Outcome::Clean;
    };
    if again.processes {
        return Outcome::Quarantined { why: again.why };
    }
    tracing::warn!(why = %again.why, "the clean of a slot failed twice; making its user again");
    if let Err(why) = os.recreate() {
        return Outcome::Quarantined {
            why: format!("{}; making the user again: {why}", again.why),
        };
    }
    match attempt(os) {
        Ok(()) => Outcome::Recreated { why: again.why },
        Err(after) => Outcome::Quarantined {
            why: format!("after the user was made again: {}", after.why),
        },
    }
}

/// Every step, then the check's two looks.
fn attempt(os: &dyn Os) -> Result<(), Failed> {
    let mut failed = Vec::new();
    for step in STEPS {
        if let Err(why) = os.run(step) {
            failed.push(format!("{step}: {why}"));
            if step == Step::Kill {
                return Err(Failed {
                    processes: true,
                    why: failed.join("; "),
                });
            }
        }
    }
    let mut left = os.left();
    if left.is_empty() {
        os.pause();
        left = os.left();
    }
    if failed.is_empty() && left.is_empty() {
        return Ok(());
    }
    let processes = left.iter().any(|l| l.step == Step::Kill);
    failed.extend(
        left.into_iter()
            .map(|l| format!("left ({}): {}", l.step, l.what)),
    );
    Err(Failed {
        processes,
        why: failed.join("; "),
    })
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, VecDeque};

    use super::*;

    /// What the fake was asked to do, in order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Run(Step),
        Left,
        Pause,
        Recreate,
    }

    /// An [`Os`] that records its calls: each step fails as many times as `fails` says,
    /// each look answers the next entry of `looks` (then nothing is left), and the
    /// recreate fails when `recreate` says so.
    #[derive(Default)]
    struct Fake {
        calls: RefCell<Vec<Call>>,
        fails: RefCell<BTreeMap<Step, usize>>,
        looks: RefCell<VecDeque<Vec<Left>>>,
        recreate: Option<String>,
    }

    impl Fake {
        fn failing(step: Step, times: usize) -> Self {
            let fake = Self::default();
            fake.fails.borrow_mut().insert(step, times);
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
            let mut fails = self.fails.borrow_mut();
            match fails.get_mut(&step) {
                Some(n) if *n > 0 => {
                    *n -= 1;
                    Err(format!("{step} failed"))
                }
                _ => Ok(()),
            }
        }

        fn left(&self) -> Vec<Left> {
            self.calls.borrow_mut().push(Call::Left);
            self.looks.borrow_mut().pop_front().unwrap_or_default()
        }

        fn pause(&self) {
            self.calls.borrow_mut().push(Call::Pause);
        }

        fn recreate(&self) -> Result<(), String> {
            self.calls.borrow_mut().push(Call::Recreate);
            self.recreate.clone().map_or(Ok(()), Err)
        }
    }

    /// The calls of one attempt that runs every step and looks twice.
    fn whole_attempt() -> Vec<Call> {
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

    /// Catches a step skipped or run out of the design's order (a kill before the
    /// bootout lets launchd start the uid's agents again; a wipe before the kill races
    /// the uid's processes), a check that looks only once (a job started again after
    /// the kill is missed), and a recreate on the normal path: a clean slot runs each
    /// step once in order, looks twice a pause apart, and is clean.
    #[test]
    fn a_clean_slot_runs_every_step_in_order_and_looks_twice() {
        assert_eq!(
            STEPS.map(|s| s.to_string()),
            [
                "bootout",
                "schedules",
                "kill",
                "images",
                "ipc",
                "home",
                "folders",
                "shared"
            ]
        );
        let fake = Fake::default();
        assert_eq!(clean(&fake), Outcome::Clean);
        assert_eq!(fake.calls(), whole_attempt());
    }

    /// Catches a clean whose check is ignored (what a step left would reach the next
    /// lease), and a check's second look ignored (what came back after the pause):
    /// a slot with something left at either look is cleaned again, and passes then
    /// without a recreate.
    #[test]
    fn what_either_look_finds_fails_the_attempt_and_the_retry_can_pass() {
        let fake = Fake::looking(vec![vec![left(Step::Home, "a file")]]);
        assert_eq!(clean(&fake), Outcome::Clean);
        let mut want = STEPS.iter().copied().map(Call::Run).collect::<Vec<_>>();
        want.push(Call::Left);
        want.extend(whole_attempt());
        assert_eq!(fake.calls(), want);

        let fake = Fake::looking(vec![vec![], vec![left(Step::Bootout, "an agent")]]);
        assert_eq!(clean(&fake), Outcome::Clean);
        let mut want = whole_attempt();
        want.extend(whole_attempt());
        assert_eq!(fake.calls(), want);
    }

    fn quarantined(why: &str) -> Outcome {
        Outcome::Quarantined {
            why: why.to_owned(),
        }
    }

    /// Catches a failed step that does not fail the clean, a step failure that stops
    /// the steps after it (one stuck image would leave the home unwiped), and a clean
    /// that never makes the user again: a step that fails twice leads to one recreate,
    /// and a clean after it that passes is a recreate, reported with why.
    #[test]
    fn a_step_that_fails_twice_makes_the_user_again() {
        let fake = Fake::failing(Step::Images, 2);
        assert_eq!(
            clean(&fake),
            Outcome::Recreated {
                why: "images: images failed".to_owned()
            }
        );
        let mut want = whole_attempt();
        want.extend(whole_attempt());
        want.push(Call::Recreate);
        want.extend(whole_attempt());
        assert_eq!(fake.calls(), want);
    }

    /// Catches a recreate of a user whose processes survived (they would run on as an
    /// orphan uid beside the next lease), steps that run after a failed kill (sweeps
    /// racing live processes), and a slot given back with survivors: a kill that fails
    /// twice, or processes the check finds, quarantine the slot without a recreate.
    #[test]
    fn survivors_quarantine_the_slot_without_a_recreate() {
        let fake = Fake::failing(Step::Kill, 2);
        assert_eq!(clean(&fake), quarantined("kill: kill failed"));
        let once = [Step::Bootout, Step::Schedules, Step::Kill].map(Call::Run);
        assert_eq!(fake.calls(), [once.clone(), once].concat());

        let survivor = || vec![left(Step::Kill, "pid 7"), left(Step::Home, "a file")];
        let fake = Fake::looking(vec![survivor(), survivor()]);
        assert_eq!(
            clean(&fake),
            quarantined("left (kill): pid 7; left (home): a file")
        );
        assert!(!fake.calls().contains(&Call::Recreate));
    }

    /// Catches a recreate decided by the first attempt's failure rather than the
    /// second's: a first attempt that fails on the home, then a retry whose check finds
    /// a live process of the uid, quarantines the slot without a recreate (the user
    /// deleted under that process would leave it running as an orphan uid).
    #[test]
    fn survivors_at_the_retry_quarantine_the_slot_after_another_first_failure() {
        let fake = Fake::failing(Step::Home, 1);
        *fake.looks.borrow_mut() = vec![vec![], vec![], vec![left(Step::Kill, "pid 7")]].into();
        assert_eq!(clean(&fake), quarantined("left (kill): pid 7"));
        let mut want = whole_attempt();
        want.extend(STEPS.iter().copied().map(Call::Run));
        want.push(Call::Left);
        assert_eq!(fake.calls(), want);
        assert!(!fake.calls().contains(&Call::Recreate));
    }

    /// Catches a recreate decision that remembers the first attempt's failed kill
    /// (`again.processes || first.processes`): a first attempt whose kill failed, then
    /// a retry whose kill succeeded and found no survivor but failed on the home, makes
    /// the user again, because no process of the uid lives at the second attempt.
    #[test]
    fn a_kill_that_failed_only_at_the_first_attempt_does_not_quarantine_the_slot() {
        let fake = Fake::failing(Step::Kill, 1);
        fake.fails.borrow_mut().insert(Step::Home, 1);
        assert_eq!(
            clean(&fake),
            Outcome::Recreated {
                why: "home: home failed".to_owned()
            }
        );
        // The first attempt stops at the failed kill, the retry runs every step and
        // looks twice, the recreate follows and the attempt after it is whole.
        let mut want = [Step::Bootout, Step::Schedules, Step::Kill]
            .map(Call::Run)
            .to_vec();
        want.extend(whole_attempt());
        want.push(Call::Recreate);
        want.extend(whole_attempt());
        assert_eq!(fake.calls(), want);
    }

    /// Catches a slot given back although its user could not be made again, or was
    /// made again and still not clean: either quarantines it, saying why.
    #[test]
    fn a_failed_recreate_or_a_failed_clean_after_it_quarantines_the_slot() {
        let mut fake = Fake::failing(Step::Home, 2);
        fake.recreate = Some("rename: Operation not permitted".to_owned());
        assert_eq!(
            clean(&fake),
            quarantined(
                "home: home failed; making the user again: rename: Operation not permitted"
            )
        );
        assert_eq!(fake.calls().last(), Some(&Call::Recreate));

        let fake = Fake::failing(Step::Shared, 3);
        assert_eq!(
            clean(&fake),
            quarantined("after the user was made again: shared: shared failed")
        );
        let recreates = fake
            .calls()
            .iter()
            .filter(|c| **c == Call::Recreate)
            .count();
        assert_eq!(recreates, 1);
    }
}
