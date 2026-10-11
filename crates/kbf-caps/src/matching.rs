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
//! | `os`, `os_image`, `cpu.model`, `page_size`, `label.<k>` | exact |
//! | `xcode` | membership: the node reports a set (one entry per installed Xcode build) and the request names one |
//! | `vm.image` | membership on the digest: the request names `<name>@sha256:<64 hex>`, the node reports one entry in that form per image it holds, and only the `sha256:<hex>` parts are compared |
//!
//! `gpu` is a count of whole GPUs (`gpu=1`). Matching compares it with the node's
//! count; the scheduler also books it, so a GPU serves one lease at a time.
//!
//! `vm.image` names a VM image by the digest of the recipe it is built from, so two
//! names for one recipe are one image, and one name for two recipes is two. A value
//! without `@sha256:<64 lowercase hex digits>` is refused, in a request and in a report.
//!
//! The report-only keys ([`REPORT_ONLY_KEYS`]: `vm.slots`, `vm.max_cpus`,
//! `vm.max_mem_gib`) are whole numbers a node reports about what it can run; a request
//! that names one is refused ([`RequestError::ReportOnly`]): a request never asks for VM
//! slots by count. Nothing books VM slots yet: the planned VM lease kind
//! (`kbf-lease=vm`) is refused as an unknown kind.
//!
//! The reserved keys ([`RESERVED_KEYS`]: `kbf-lease`, `kbf-cpu`, `kbf-mac-admin`,
//! `kbf-book-cpus`, `kbf-book-mem-gib`) ask for a kind or a size of capacity, not a
//! capability; [`Request::parse`] skips them for the scheduler.
//!
//! The iOS device keys, `ios.device` and every `ios.device.<attribute>` key, are
//! planned (`docs/design/ios-devices.md`). Until the scheduler books devices,
//! [`Request::parse`] refuses each of them ([`RequestError::IosDevice`]): ignored, as an
//! unknown property is, `ios.device=1` would match every node, Linux included.

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

    /// The consumable whose key is `key`, if any.
    pub(crate) fn from_name(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.name() == key)
    }

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
const EXACT_KEYS: [&str; 4] = ["os", "os_image", "cpu.model", "page_size"];

/// Keys a node reports as a set and a request names one member of.
const MEMBER_KEYS: [&str; 2] = ["xcode", VM_IMAGE_KEY];

/// The VM image key: membership on the `sha256:<hex>` part of the value.
const VM_IMAGE_KEY: &str = "vm.image";

/// Keys a node reports and a request may not name: whole numbers about the VMs a node
/// can run.
pub const REPORT_ONLY_KEYS: [&str; 3] = ["vm.slots", "vm.max_cpus", "vm.max_mem_gib"];

/// Reserved keys: they ask for a kind or a size of capacity, not a capability, and are
/// read by the front, not matched.
pub const RESERVED_KEYS: [&str; 5] = [
    "kbf-lease",
    "kbf-cpu",
    "kbf-mac-admin",
    "kbf-book-cpus",
    "kbf-book-mem-gib",
];

/// The iOS device key; every `ios.device.<attribute>` key begins with it and a `.`.
const IOS_DEVICE_KEY: &str = "ios.device";

/// Whether `key` is `ios.device` or `ios.device.<anything>`: planned, and refused until
/// the scheduler books devices. `ios.devices` and `ios.simulator` are not device keys.
pub(crate) fn is_ios_device_key(key: &str) -> bool {
    key.strip_prefix(IOS_DEVICE_KEY)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// Whether `key` is compared as an exact string: one of the exact keys, or
/// `label.<k>` with a non-empty `<k>`.
pub(crate) fn is_exact_key(key: &str) -> bool {
    EXACT_KEYS.contains(&key) || key.strip_prefix("label.").is_some_and(|k| !k.is_empty())
}

/// Whether `key` names a set the node reports, matched by membership.
pub(crate) fn is_member_key(key: &str) -> bool {
    MEMBER_KEYS.contains(&key)
}

/// What membership compares for a value of member key `key`: the `sha256:<hex>` part of
/// a `vm.image` value (`None` if it has none), and any other key's whole value.
pub(crate) fn member_token<'v>(key: &str, value: &'v str) -> Option<&'v str> {
    if key == VM_IMAGE_KEY {
        image_digest(value)
    } else {
        Some(value)
    }
}

/// The `sha256:<hex>` part of `<name>@sha256:<hex>`: a non-empty name with no `@` or
/// whitespace, and exactly 64 lowercase hex digits.
fn image_digest(value: &str) -> Option<&str> {
    let (name, digest) = value.split_once('@')?;
    let hex = digest.strip_prefix("sha256:")?;
    let name_ok = !name.is_empty() && !name.contains(char::is_whitespace);
    let hex_ok = hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (name_ok && hex_ok).then_some(digest)
}

/// Whether `key` is one of kbf's own platform keys: a capability key, a reserved key
/// [`Request::parse`] skips, or a report-only or iOS device key it refuses.
pub(crate) fn is_own_key(key: &str) -> bool {
    is_capability_key(key)
        || RESERVED_KEYS.contains(&key)
        || REPORT_ONLY_KEYS.contains(&key)
        || is_ios_device_key(key)
}

/// Whether [`Request::parse`] reads `key` as a capability (reserved keys are not).
fn is_capability_key(key: &str) -> bool {
    matches!(key, "arch" | "isa_level" | "cpu.feature")
        || Consumable::from_name(key).is_some()
        || is_exact_key(key)
        || is_member_key(key)
}

/// What a node offers, as the scheduler sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeCaps {
    /// Detected CPU capabilities.
    pub cpu: CpuCaps,
    /// Values compared exactly, by request key (`os`, `cpu.model`, `label.rack`, ...).
    pub exact: BTreeMap<String, String>,
    /// Sets a request names one member of, by request key (`xcode`: every installed
    /// build; `vm.image`: the `sha256:<hex>` digest of every image).
    pub members: BTreeMap<String, BTreeSet<String>>,
    /// Countable capacity. A missing entry counts as zero.
    pub consumables: BTreeMap<Consumable, u64>,
    /// The drivers the node's daemon runs (`container`, `native`, ...), from its
    /// report's `drivers` entries. No request key matches them; the scheduler reads
    /// them to place a lease kind only where a driver serves it.
    pub drivers: BTreeSet<String>,
}

impl NodeCaps {
    /// A node with only CPU capabilities: no exact values, no sets, no capacity, no
    /// driver.
    #[must_use]
    pub fn new(cpu: CpuCaps) -> Self {
        Self {
            cpu,
            exact: BTreeMap::new(),
            members: BTreeMap::new(),
            consumables: BTreeMap::new(),
            drivers: BTreeSet::new(),
        }
    }

    /// This node, running `drivers` as well.
    #[must_use]
    pub fn with_drivers<'d>(mut self, drivers: impl IntoIterator<Item = &'d str>) -> Self {
        self.drivers.extend(drivers.into_iter().map(str::to_owned));
        self
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
    /// A `vm.image` value without a digest.
    #[error(
        "vm.image {0:?} names no digest; name an image as <name>@sha256:<64 lowercase hex digits>"
    )]
    NoImageDigest(String),
    /// The key is one a node reports and a request may not name.
    #[error("{0:?} is reported by a node and cannot be requested")]
    ReportOnly(String),
    /// An iOS device key (`ios.device`, `ios.device.<attribute>`), refused until the
    /// scheduler books devices.
    #[error(
        "{0:?} is refused until kbf books iOS devices; the planned design is \
         docs/design/ios-devices.md#55-rollout-order"
    )]
    IosDevice(String),
}

/// What an action requires of a node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    arch: Option<Arch>,
    isa_level: Option<IsaLevel>,
    features: BTreeSet<String>,
    pub(crate) exact: BTreeMap<String, String>,
    /// By key: the value as requested, and what membership compares.
    members: BTreeMap<String, (String, String)>,
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
    /// The node's set under `key` does not hold `want` (for `vm.image`, its digest).
    Member {
        key: &'a str,
        want: &'a str,
    },
    Consumable {
        what: Consumable,
        want: u64,
        have: u64,
    },
}

impl fmt::Display for Unmet<'_> {
    /// The requirement in request syntax: `arch=arm64`, `isa_level>=x86-64-v3`,
    /// `cpu.feature=avx2`, `os=macos`, `xcode=16C5032a`, `gpu>=2 (has 1)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arch { want } => write!(f, "arch={want}"),
            Self::IsaLevel { want } => write!(f, "isa_level>={want}"),
            Self::Feature(feature) => write!(f, "cpu.feature={feature}"),
            Self::Exact { key, want } | Self::Member { key, want } => write!(f, "{key}={want}"),
            Self::Consumable { what, want, have } => write!(f, "{what}>={want} (has {have})"),
        }
    }
}

impl Request {
    /// Builds a request from `(key, value)` properties; see the module table.
    ///
    /// `cpu.feature` may repeat; every other key may appear once. A membership key
    /// (`xcode`, `vm.image`) names one non-empty value; a `vm.image` value carries a
    /// digest. A report-only key, and an iOS device key, is refused.
    pub fn parse<'p, I>(properties: I) -> Result<Self, RequestError>
    where
        I: IntoIterator<Item = (&'p str, &'p str)>,
    {
        Self::parse_each(&mut properties.into_iter())
    }

    /// [`Self::parse`], compiled once rather than once per caller's iterator type.
    fn parse_each<'p>(
        properties: &mut dyn Iterator<Item = (&'p str, &'p str)>,
    ) -> Result<Self, RequestError> {
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
            if REPORT_ONLY_KEYS.contains(&key) {
                return Err(RequestError::ReportOnly(key.to_owned()));
            }
            if is_ios_device_key(key) {
                return Err(RequestError::IosDevice(key.to_owned()));
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
            } else if let Some(what) = Consumable::from_name(key) {
                let amount = value.parse().map_err(|_| bad())?;
                if req.minimums.insert(what, amount).is_some() {
                    return Err(repeated());
                }
            } else if is_exact_key(key) {
                if req.exact.insert(key.to_owned(), value.to_owned()).is_some() {
                    return Err(repeated());
                }
            } else if is_member_key(key) {
                if value.is_empty() {
                    return Err(bad());
                }
                let token = member_token(key, value)
                    .ok_or_else(|| RequestError::NoImageDigest(value.to_owned()))?;
                let member = (value.to_owned(), token.to_owned());
                if req.members.insert(key.to_owned(), member).is_some() {
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
        for (key, (want, token)) in &self.members {
            if !node.members.get(key).is_some_and(|set| set.contains(token)) {
                unmet.push(Unmet::Member { key, want });
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
