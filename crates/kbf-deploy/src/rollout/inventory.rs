//! The hosts a rollout upgrades, and the order it upgrades them in.
//!
//! The inventory file is one host per line, `NAME ROLE`; blank lines and lines starting
//! with `#` are skipped. A role is one of:
//!
//! - `storage-and-daemon`: a converged storage node (storage and daemon in one process
//!   set), rolled one at a time with the replication gate;
//! - `frontend`: the one frontend (exactly one is required: it holds the state store the
//!   backup copies, and answers the fleet lock and the cluster's counts);
//! - `daemon`: a Linux host running only a daemon;
//! - `daemon-mac`: a Mac running a daemon.
//!
//! The order of a rollout comes from the roles, not from the file: every
//! `storage-and-daemon` host, then the frontend, then every `daemon` host, then every
//! `daemon-mac` host. The file's order only orders hosts of one role.

use std::fmt;

use super::transport::{HostName, NameError};

/// What a host runs. The declaration order is the rollout order ([`Inventory::ordered`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    StorageAndDaemon,
    Frontend,
    Daemon,
    DaemonMac,
}

impl Role {
    fn parse(word: &str) -> Option<Role> {
        match word {
            "storage-and-daemon" => Some(Role::StorageAndDaemon),
            "frontend" => Some(Role::Frontend),
            "daemon" => Some(Role::Daemon),
            "daemon-mac" => Some(Role::DaemonMac),
            _ => None,
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Role::StorageAndDaemon => "storage-and-daemon",
            Role::Frontend => "frontend",
            Role::Daemon => "daemon",
            Role::DaemonMac => "daemon-mac",
        })
    }
}

/// One host of the inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: HostName,
    pub role: Role,
}

/// Why an inventory was refused. Line numbers count from 1.
#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("line {line}: expected `NAME ROLE`")]
    Malformed { line: usize },
    #[error("line {line}: {source}")]
    Name { line: usize, source: NameError },
    #[error(
        "line {line}: unknown role {role:?} (storage-and-daemon, frontend, daemon or daemon-mac)"
    )]
    Role { line: usize, role: String },
    #[error("line {line}: host {name} is already listed on line {first}")]
    Repeated {
        line: usize,
        name: HostName,
        first: usize,
    },
    #[error("the inventory needs exactly one frontend, and has {found}")]
    Frontends { found: usize },
}

/// A checked inventory: names unique, exactly one frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inventory {
    hosts: Vec<Host>,
    frontend: HostName,
}

impl Inventory {
    pub fn parse(text: &str) -> Result<Inventory, InventoryError> {
        let mut hosts: Vec<(usize, Host)> = Vec::new();
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let raw = raw.trim();
            if raw.is_empty() || raw.starts_with('#') {
                continue;
            }
            let words: Vec<&str> = raw.split_whitespace().collect();
            let [name, role] = words[..] else {
                return Err(InventoryError::Malformed { line });
            };
            let name =
                HostName::new(name).map_err(|source| InventoryError::Name { line, source })?;
            let role = Role::parse(role).ok_or_else(|| InventoryError::Role {
                line,
                role: role.to_owned(),
            })?;
            if let Some((first, _)) = hosts.iter().find(|(_, h)| h.name == name) {
                return Err(InventoryError::Repeated {
                    line,
                    name,
                    first: *first,
                });
            }
            hosts.push((line, Host { name, role }));
        }
        let hosts: Vec<Host> = hosts.into_iter().map(|(_, h)| h).collect();
        let frontends: Vec<&Host> = hosts.iter().filter(|h| h.role == Role::Frontend).collect();
        let [frontend] = frontends[..] else {
            return Err(InventoryError::Frontends {
                found: frontends.len(),
            });
        };
        let frontend = frontend.name.clone();
        Ok(Inventory { hosts, frontend })
    }

    /// The frontend: the host the backup, the lock and the cluster's counts are asked
    /// of.
    pub fn frontend(&self) -> &HostName {
        &self.frontend
    }

    /// The hosts in rollout order: by role, then by the file's order.
    pub fn ordered(&self) -> Vec<Host> {
        let mut hosts = self.hosts.clone();
        hosts.sort_by_key(|h| h.role);
        hosts
    }
}

#[cfg(test)]
#[path = "inventory_tests.rs"]
mod tests;
