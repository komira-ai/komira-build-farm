//! Matching an action's capability request against a node's capabilities.
//!
//! Each request key has one typed comparison:
//!
//! | Key | Comparison |
//! |---|---|
//! | `arch` | exact |
//! | `isa_level` | at least, within the family (`x86-64-v3` is served by v3 and v4) |
//! | `cpu.feature` (repeatable) | subset: every requested feature is present |
//! | `cpus`, `mem_gib`, `nvme_gib`, `gpu` | countable: the node has at least the amount |
//! | `os`, `os_image`, `cpu.model`, `page_size`, `xcode`, `label.<k>` | exact |
//!
//! `gpu` is a count of whole GPUs (`gpu=1`). Matching compares it with the node's
//! count; the scheduler also books it, so a GPU serves one lease at a time.
//!
//! The reserved `kbf-lease`, `kbf-cpu` and `kbf-mac-admin` keys ask for a kind of
//! capacity, not a capability; [`Request::parse`] skips them for the scheduler.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::cpu::{Arch, CpuCaps};
use crate::level::IsaLevel;

/// A countable resource a request asks a minimum of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Consumable {
    Cpus,
    MemGib,
    NvmeGib,
    /// Whole GPUs.
    Gpus,
}

impl Consumable {
    const ALL: [Self; 4] = [Self::Cpus, Self::MemGib, Self::NvmeGib, Self::Gpus];

    /// The request key.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpus => "cpus",
            Self::MemGib => "mem_gib",
            Self::NvmeGib => "nvme_gib",
            Self::Gpus => "gpu",
        }
    }
}

impl fmt::Display for Consumable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Keys compared as exact strings, besides `label.<k>`.
const EXACT_KEYS: [&str; 5] = ["os", "os_image", "cpu.model", "page_size", "xcode"];

/// Reserved keys that are not capabilities.
const RESERVED_KEYS: [&str; 3] = ["kbf-lease", "kbf-cpu", "kbf-mac-admin"];

/// What a node offers, as the scheduler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCaps {
    /// Detected CPU capabilities.
    pub cpu: CpuCaps,
    /// Values compared exactly, by request key (`os`, `cpu.model`, `label.rack`, ...).
    pub exact: BTreeMap<String, String>,
    /// Countable capacity. A missing entry counts as zero.
    pub consumables: BTreeMap<Consumable, u64>,
}

impl NodeCaps {
    /// A node with only CPU capabilities: no exact values, no capacity.
    #[must_use]
    pub fn new(cpu: CpuCaps) -> Self {
        Self {
            cpu,
            exact: BTreeMap::new(),
            consumables: BTreeMap::new(),
        }
    }
}

/// Why a set of request properties is not a valid [`Request`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The key is not a capability key.
    #[error("unknown capability key {0:?}")]
    UnknownKey(String),
    /// The value is not valid for the key.
    #[error("invalid value {value:?} for capability key {key:?}")]
    BadValue { key: String, value: String },
    /// A key that takes one value appears more than once.
    #[error("capability key {0:?} appears more than once")]
    Repeated(String),
}

/// What an action requires of a node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    arch: Option<Arch>,
    isa_level: Option<IsaLevel>,
    features: BTreeSet<String>,
    exact: BTreeMap<String, String>,
    minimums: BTreeMap<Consumable, u64>,
}

/// One requirement a node does not meet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmet<'a> {
    Arch {
        want: Arch,
    },
    IsaLevel {
        want: IsaLevel,
    },
    Feature(&'a str),
    Exact {
        key: &'a str,
        want: &'a str,
    },
    Consumable {
        what: Consumable,
        want: u64,
        have: u64,
    },
}

impl Request {
    /// Builds a request from `(key, value)` properties; see the module table.
    ///
    /// `cpu.feature` may repeat; every other key may appear once.
    pub fn parse<'p, I>(properties: I) -> Result<Self, RequestError>
    where
        I: IntoIterator<Item = (&'p str, &'p str)>,
    {
        let mut req = Self::default();
        for (key, value) in properties {
            let bad = || RequestError::BadValue {
                key: key.to_owned(),
                value: value.to_owned(),
            };
            let repeated = || RequestError::Repeated(key.to_owned());
            if RESERVED_KEYS.contains(&key) {
                continue;
            }
            if key == "arch" {
                let arch = value.parse().map_err(|_| bad())?;
                set_once(&mut req.arch, arch).ok_or_else(repeated)?;
            } else if key == "isa_level" {
                let level = value.parse().map_err(|_| bad())?;
                set_once(&mut req.isa_level, level).ok_or_else(repeated)?;
            } else if key == "cpu.feature" {
                if value.is_empty() || value.contains(char::is_whitespace) {
                    return Err(bad());
                }
                req.features.insert(value.to_owned());
            } else if let Some(what) = Consumable::ALL.into_iter().find(|c| c.name() == key) {
                let amount = value.parse().map_err(|_| bad())?;
                if req.minimums.insert(what, amount).is_some() {
                    return Err(repeated());
                }
            } else if EXACT_KEYS.contains(&key)
                || key.strip_prefix("label.").is_some_and(|k| !k.is_empty())
            {
                if req.exact.insert(key.to_owned(), value.to_owned()).is_some() {
                    return Err(repeated());
                }
            } else {
                return Err(RequestError::UnknownKey(key.to_owned()));
            }
        }
        Ok(req)
    }

    /// Every requirement the node does not meet, in a stable order. Empty means the
    /// node matches.
    #[must_use]
    pub fn unmet(&self, node: &NodeCaps) -> Vec<Unmet<'_>> {
        let mut unmet = Vec::new();
        if let Some(want) = self.arch
            && node.cpu.arch() != want
        {
            unmet.push(Unmet::Arch { want });
        }
        if let Some(want) = self.isa_level
            && !node.cpu.level().is_some_and(|have| have.satisfies(want))
        {
            unmet.push(Unmet::IsaLevel { want });
        }
        unmet.extend(
            self.features
                .iter()
                .filter(|f| !node.cpu.has(f))
                .map(|f| Unmet::Feature(f)),
        );
        for (key, want) in &self.exact {
            if node.exact.get(key) != Some(want) {
                unmet.push(Unmet::Exact { key, want });
            }
        }
        for (&what, &want) in &self.minimums {
            let have = node.consumables.get(&what).copied().unwrap_or(0);
            if have < want {
                unmet.push(Unmet::Consumable { what, want, have });
            }
        }
        unmet
    }

    /// Whether the node meets every requirement.
    #[must_use]
    pub fn matches(&self, node: &NodeCaps) -> bool {
        self.unmet(node).is_empty()
    }
}

/// Sets `slot` if empty. `None` if it was already set.
fn set_once<T>(slot: &mut Option<T>, value: T) -> Option<()> {
    if slot.is_some() {
        return None;
    }
    *slot = Some(value);
    Some(())
}
