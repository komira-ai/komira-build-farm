//! The rollout state machine.
//!
//! [`run`] takes the fleet lock, backs up the state store, then upgrades the hosts of
//! the [`Plan`] one at a time and stops at the first that does not go right. Per host:
//!
//! ```text
//! gate        no under-replicated blob in the cluster       (refused, nothing touched)
//! stage       files on the host, verified
//! drain       stop new work, then wait for the work in flight to end
//!                                                            (undrain, then stop)
//! swap        make the new commit the running one
//! wait        healthy at the new commit; a storage node also until no blob is
//!             under-replicated                              (any failure: rollback)
//! confirm     cancel the host's own revert timer
//! undrain
//! ```
//!
//! Three ends other than success:
//!
//! - [`Outcome::Pending`]: the work in flight did not end in time. The host is undrained
//!   and as it was; run again later. Not a failure.
//! - [`Outcome::Paused`]: the pause control was set. It is read before the run and
//!   between hosts, never in the middle of one, so no host is left drained.
//! - [`Outcome::Failed`]: a gate, a verb or a wait failed. A host that was swapped (or
//!   might have been: an error from [`Transport::swap`] counts) is put back with
//!   [`Transport::rollback`] and undrained. Nothing after it is touched. A host that
//!   cannot be put back, or undrained, stays drained ([`Recovery::Cordoned`]) for a
//!   person.
//!
//! The order the hosts run in is the plan's, which comes from the roles
//! ([`super::inventory`]); at most one host is drained at a time, because a host is
//! undrained (or cordoned, and the run over) before the next is touched.
//!
//! Time never comes from a clock: a wait polls every [`POLL`] and passes the interval
//! to [`Transport::sleep`], and its limit is the number of intervals that fit in it.
//! Progress lines are best effort: a failed write to `out` never leaves a host drained.

use std::fmt;
use std::io::Write;
use std::time::Duration;

use super::inventory::{Host, Role};
use super::plan::{Plan, Step};
use super::transport::{Commit, HostName, Lock, RunId, Transport, TransportError};

/// How long a wait lets pass between two looks.
pub const POLL: Duration = Duration::from_secs(5);

/// How long each wait may last before the rollout gives up on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Leases on a Linux daemon to finish (15 minutes: never kill one).
    pub linux_drain: Duration,
    /// Leases on a Mac daemon to finish (60 minutes).
    pub mac_drain: Duration,
    /// Open Execute streams on the frontend to end (6 hours: Buck2 does not recover a
    /// stream that breaks after it started).
    pub frontend_idle: Duration,
    /// A swapped host to be healthy (120 seconds).
    pub health: Duration,
    /// A swapped storage node's blobs to be back on all their copies (5 minutes). With
    /// `health` it must stay inside the host's commit-confirm timer.
    pub repair: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            linux_drain: Duration::from_secs(15 * 60),
            mac_drain: Duration::from_secs(60 * 60),
            frontend_idle: Duration::from_secs(6 * 60 * 60),
            health: Duration::from_secs(120),
            repair: Duration::from_secs(5 * 60),
        }
    }
}

impl Limits {
    fn drain(&self, role: Role) -> Duration {
        match role {
            Role::StorageAndDaemon | Role::Daemon => self.linux_drain,
            Role::Frontend => self.frontend_idle,
            Role::DaemonMac => self.mac_drain,
        }
    }
}

/// Where a failed host was left.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Nothing on it changed.
    Untouched,
    /// Drained, then undrained, never swapped.
    Undrained,
    /// Swapped (or maybe), put back on the previous commit, and undrained.
    RolledBack,
    /// Still drained: a person must look. The text says what could not be done.
    Cordoned(String),
}

impl fmt::Display for Recovery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Recovery::Untouched => f.write_str("the host was not changed"),
            Recovery::Undrained => f.write_str("the host was undrained, not swapped"),
            Recovery::RolledBack => f.write_str("the host was rolled back and undrained"),
            Recovery::Cordoned(why) => write!(f, "the host stays drained: {why}"),
        }
    }
}

/// The host a rollout stopped at, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub host: HostName,
    pub why: String,
    pub recovery: Recovery,
}

/// How a rollout that took the lock ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every host is on the commit.
    Done {
        hosts: usize,
    },
    /// The pause control was set after `done` hosts.
    Paused {
        done: usize,
    },
    /// `host` still had work in flight when the wait ended; it is as it was.
    Pending {
        host: HostName,
        why: String,
    },
    Failed(Failure),
}

/// Why a rollout did not start.
#[derive(Debug, thiserror::Error)]
pub enum RolloutError {
    #[error("the fleet is locked by run {by}")]
    Locked { by: RunId },
    #[error("cannot take the fleet lock: {0}")]
    Lock(#[from] TransportError),
}

/// Why one step of a host's upgrade did not go right.
#[derive(Debug)]
enum Why {
    Transport(TransportError),
    Timeout {
        waiting_for: &'static str,
        limit: Duration,
        last: String,
    },
    Degraded(u64),
}

impl From<TransportError> for Why {
    fn from(e: TransportError) -> Why {
        Why::Transport(e)
    }
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Why::Transport(e) => e.fmt(f),
            Why::Timeout {
                waiting_for,
                limit,
                last,
            } => write!(
                f,
                "{waiting_for}: not done within {} s ({last})",
                limit.as_secs()
            ),
            Why::Degraded(n) => write!(f, "{n} blobs are under-replicated"),
        }
    }
}

/// What a wait polls for.
enum Until<'a> {
    Idle(&'a HostName),
    Healthy(&'a HostName, &'a Commit),
    Replicated,
}

struct Driver<'a> {
    plan: &'a Plan,
    tx: &'a mut dyn Transport,
    limits: &'a Limits,
    out: &'a mut dyn Write,
}

/// Roll `plan` out through `tx`, writing a `+ <step>` line as each step starts.
///
/// `paused` is asked before anything is touched and again before each host.
pub fn run(
    plan: &Plan,
    tx: &mut dyn Transport,
    limits: &Limits,
    paused: &dyn Fn() -> bool,
    out: &mut dyn Write,
) -> Result<Outcome, RolloutError> {
    if paused() {
        return Ok(Outcome::Paused { done: 0 });
    }
    let mut driver = Driver {
        plan,
        tx,
        limits,
        out,
    };
    driver.lock()?;
    let outcome = driver.rollout(paused);
    driver.unlock();
    Ok(outcome)
}

impl Driver<'_> {
    fn say(&mut self, step: &Step) {
        let _ = writeln!(self.out, "+ {step}");
    }

    fn lock(&mut self) -> Result<(), RolloutError> {
        let run = self.plan.run.clone();
        self.say(&Step::Lock { run: run.clone() });
        match self.tx.lock(self.plan.frontend(), &run)? {
            Lock::Acquired => Ok(()),
            Lock::Held { by } => Err(RolloutError::Locked { by }),
        }
    }

    fn unlock(&mut self) {
        self.say(&Step::Unlock);
        if let Err(e) = self.tx.unlock(self.plan.frontend(), &self.plan.run) {
            let _ = writeln!(self.out, "! the lock was not released: {e}");
        }
    }

    fn rollout(&mut self, paused: &dyn Fn() -> bool) -> Outcome {
        let plan = self.plan;
        self.say(&Step::Backup {
            frontend: plan.frontend().clone(),
        });
        if let Err(e) = self.tx.backup_state(plan.frontend()) {
            return failed(plan.frontend(), &e, Recovery::Untouched);
        }
        for (done, host) in plan.hosts().iter().enumerate() {
            if paused() {
                return Outcome::Paused { done };
            }
            if let Err(outcome) = self.upgrade(host) {
                return outcome;
            }
        }
        Outcome::Done {
            hosts: plan.hosts().len(),
        }
    }

    /// One host, through the state machine in the module docs.
    fn upgrade(&mut self, host: &Host) -> Result<(), Outcome> {
        if let Err(why) = self.prepare(host) {
            return Err(failed(&host.name, &why, Recovery::Untouched));
        }
        if let Err(why) = self.quiesce(host) {
            return Err(self.abandon(host, &why));
        }
        if let Err(why) = self.replace(host) {
            return Err(self.revert(host, &why));
        }
        match self.undrain(host) {
            Ok(()) => Ok(()),
            Err(why) => Err(failed(
                &host.name,
                &why,
                Recovery::Cordoned("the new commit is confirmed and running".to_owned()),
            )),
        }
    }

    /// The gate and the staging: nothing here changes what runs.
    fn prepare(&mut self, host: &Host) -> Result<(), Why> {
        self.say(&Step::Gate {
            host: host.name.clone(),
        });
        let degraded = self.tx.under_replicated_blobs(self.plan.frontend())?;
        if degraded > 0 {
            return Err(Why::Degraded(degraded));
        }
        self.say(&Step::Stage {
            host: host.name.clone(),
            commit: self.plan.commit.clone(),
        });
        self.tx.stage(&host.name, &self.plan.commit)?;
        Ok(())
    }

    /// Drain, and wait for the work in flight to end.
    fn quiesce(&mut self, host: &Host) -> Result<(), Why> {
        self.say(&Step::Drain {
            host: host.name.clone(),
        });
        self.tx.drain(&host.name)?;
        self.say(&Step::WaitIdle {
            host: host.name.clone(),
        });
        self.wait(
            &Until::Idle(&host.name),
            self.limits.drain(host.role),
            "work in flight",
        )
    }

    /// Swap, wait for the host to be healthy, confirm.
    fn replace(&mut self, host: &Host) -> Result<(), Why> {
        let commit = &self.plan.commit;
        self.say(&Step::Swap {
            host: host.name.clone(),
            commit: commit.clone(),
        });
        self.tx.swap(&host.name, commit)?;
        self.say(&Step::WaitHealthy {
            host: host.name.clone(),
            commit: commit.clone(),
        });
        self.wait(
            &Until::Healthy(&host.name, commit),
            self.limits.health,
            "health",
        )?;
        if host.role == Role::StorageAndDaemon {
            self.say(&Step::WaitRepaired {
                host: host.name.clone(),
            });
            self.wait(&Until::Replicated, self.limits.repair, "replication")?;
        }
        self.say(&Step::Confirm {
            host: host.name.clone(),
        });
        self.tx.confirm(&host.name)?;
        Ok(())
    }

    fn undrain(&mut self, host: &Host) -> Result<(), Why> {
        self.say(&Step::Undrain {
            host: host.name.clone(),
        });
        self.tx.undrain(&host.name)?;
        Ok(())
    }

    /// The host was drained and never swapped: let it take work again. A wait that ran
    /// out is [`Outcome::Pending`] (nothing is wrong, the host was busy); anything else
    /// is a failure.
    fn abandon(&mut self, host: &Host, why: &Why) -> Outcome {
        match self.undrain(host) {
            Ok(()) if matches!(why, Why::Timeout { .. }) => Outcome::Pending {
                host: host.name.clone(),
                why: why.to_string(),
            },
            Ok(()) => failed(&host.name, why, Recovery::Undrained),
            Err(e) => failed(&host.name, why, Recovery::Cordoned(e.to_string())),
        }
    }

    /// The host was swapped, or might have been: put the previous commit back, then
    /// let the host take work again.
    fn revert(&mut self, host: &Host, why: &Why) -> Outcome {
        self.say(&Step::Rollback {
            host: host.name.clone(),
        });
        let recovery = match self.tx.rollback(&host.name) {
            Err(e) => Recovery::Cordoned(format!("rollback failed: {e}")),
            Ok(()) => match self.undrain(host) {
                Ok(()) => Recovery::RolledBack,
                Err(e) => Recovery::Cordoned(format!("rolled back, but not undrained: {e}")),
            },
        };
        failed(&host.name, why, recovery)
    }

    /// Poll `until` every [`POLL`] until it holds, or `limit` has passed.
    fn wait(
        &mut self,
        until: &Until<'_>,
        limit: Duration,
        waiting_for: &'static str,
    ) -> Result<(), Why> {
        let mut waited = Duration::ZERO;
        loop {
            let Some(last) = self.probe(until)? else {
                return Ok(());
            };
            if waited >= limit {
                return Err(Why::Timeout {
                    waiting_for,
                    limit,
                    last,
                });
            }
            self.tx.sleep(POLL);
            waited += POLL;
        }
    }

    /// One look: `None` when `until` holds, else what is still missing.
    fn probe(&mut self, until: &Until<'_>) -> Result<Option<String>, TransportError> {
        Ok(match until {
            Until::Idle(host) => match self.tx.in_flight(host)? {
                0 => None,
                n => Some(format!("{n} still in flight")),
            },
            Until::Healthy(host, commit) => {
                let health = self.tx.check(host, commit)?;
                if health.is_healthy() {
                    None
                } else {
                    Some(health.problem())
                }
            }
            Until::Replicated => match self.tx.under_replicated_blobs(self.plan.frontend())? {
                0 => None,
                n => Some(format!("{n} blobs under-replicated")),
            },
        })
    }
}

fn failed(host: &HostName, why: &dyn fmt::Display, recovery: Recovery) -> Outcome {
    Outcome::Failed(Failure {
        host: host.clone(),
        why: why.to_string(),
        recovery,
    })
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
