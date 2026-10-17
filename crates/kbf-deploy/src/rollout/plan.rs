//! The steps of a rollout, in order: what a dry run prints and what an apply announces
//! as it goes.

use std::fmt;

use super::inventory::{Host, Inventory, Role};
use super::transport::{Commit, HostName, RunId};

/// One thing the rollout does. An apply prints `+ <step>` as it starts each; a dry run
/// prints the same lines as `DRY-RUN: <step>` and does none of them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    Lock {
        run: RunId,
    },
    Backup {
        frontend: HostName,
    },
    /// The cluster has no under-replicated blob, so this host may go down.
    Gate {
        host: HostName,
    },
    Stage {
        host: HostName,
        commit: Commit,
    },
    Drain {
        host: HostName,
    },
    WaitIdle {
        host: HostName,
    },
    Swap {
        host: HostName,
        commit: Commit,
    },
    WaitHealthy {
        host: HostName,
        commit: Commit,
    },
    WaitRepaired {
        host: HostName,
    },
    Confirm {
        host: HostName,
    },
    Undrain {
        host: HostName,
    },
    /// Not in a plan: only a failure runs it.
    Rollback {
        host: HostName,
    },
    Unlock,
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Step::Lock { run } => write!(f, "lock the fleet as run {run}"),
            Step::Backup { frontend } => write!(f, "{frontend}: back up the state store"),
            Step::Gate { host } => write!(f, "{host}: require no under-replicated blob"),
            Step::Stage { host, commit } => write!(f, "{host}: stage {commit}"),
            Step::Drain { host } => write!(f, "{host}: drain"),
            Step::WaitIdle { host } => write!(f, "{host}: wait until nothing is in flight"),
            Step::Swap { host, commit } => write!(f, "{host}: swap to {commit}"),
            Step::WaitHealthy { host, commit } => {
                write!(f, "{host}: wait until healthy at {commit}")
            }
            Step::WaitRepaired { host } => {
                write!(f, "{host}: wait until no blob is under-replicated")
            }
            Step::Confirm { host } => write!(f, "{host}: confirm the swap"),
            Step::Undrain { host } => write!(f, "{host}: undrain"),
            Step::Rollback { host } => write!(f, "{host}: roll back"),
            Step::Unlock => f.write_str("unlock the fleet"),
        }
    }
}

/// What a rollout does to which hosts, in which order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub run: RunId,
    pub commit: Commit,
    frontend: HostName,
    hosts: Vec<Host>,
}

impl Plan {
    pub fn new(inventory: &Inventory, run: RunId, commit: Commit) -> Plan {
        Plan {
            run,
            commit,
            frontend: inventory.frontend().clone(),
            hosts: inventory.ordered(),
        }
    }

    pub fn frontend(&self) -> &HostName {
        &self.frontend
    }

    /// The hosts, in the order they are upgraded.
    pub fn hosts(&self) -> &[Host] {
        &self.hosts
    }

    /// Every step a rollout that goes right takes, in order.
    pub fn steps(&self) -> Vec<Step> {
        let mut steps = vec![
            Step::Lock {
                run: self.run.clone(),
            },
            Step::Backup {
                frontend: self.frontend.clone(),
            },
        ];
        for h in &self.hosts {
            let host = h.name.clone();
            let commit = self.commit.clone();
            steps.push(Step::Gate { host: host.clone() });
            steps.push(Step::Stage {
                host: host.clone(),
                commit: commit.clone(),
            });
            steps.push(Step::Drain { host: host.clone() });
            steps.push(Step::WaitIdle { host: host.clone() });
            steps.push(Step::Swap {
                host: host.clone(),
                commit: commit.clone(),
            });
            steps.push(Step::WaitHealthy {
                host: host.clone(),
                commit,
            });
            if h.role == Role::StorageAndDaemon {
                steps.push(Step::WaitRepaired { host: host.clone() });
            }
            steps.push(Step::Confirm { host: host.clone() });
            steps.push(Step::Undrain { host });
        }
        steps.push(Step::Unlock);
        steps
    }
}

#[cfg(test)]
#[path = "plan_tests.rs"]
mod tests;
